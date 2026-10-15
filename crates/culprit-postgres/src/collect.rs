use std::time::Instant;

use culprit_core::{
    Capability, ColumnInfo, ConnectionSpec, Coverage, Engine, ForeignKey, IndexStat, MissingCapability, PlanKind,
    QueryFamily, QueryStat, Result, SNAPSHOT_FORMAT, ServerInfo, Setting, Snapshot, SnapshotOptions, Spread,
    StatisticsInfo, TableStat, WaitStat, Workload,
};
use tokio_postgres::Client;

use crate::connect::{COLLECTOR, Connector, sql_state};
use crate::plan_json;
use crate::rows::{TAG, iso, query, query_one};

const PG_STAT_STATEMENTS_FIX: &str = "Add pg_stat_statements to shared_preload_libraries, restart, then run CREATE EXTENSION pg_stat_statements; in this database.";

/// What the probe learned about the server, shared by every collector.
pub struct Probe {
    pub version_num: u32,
    pub version: String,
    pub database: String,
    pub size_bytes: Option<f64>,
    pub started_at: Option<String>,
    pub now: String,
    pub user: String,
    pub privileged: bool,
    pub cloud: Option<String>,
    pub statements: StatementsAccess,
    /// The server already loads pg_stat_statements (shared_preload_libraries), so it has been recording all along and
    /// only the extension's view is missing.
    pub preloaded: bool,
    /// This login can create the extension (superuser, a cloud admin role, or CREATE on the database).
    pub can_create_extension: bool,
    /// The server is a standby replaying another's WAL (`pg_is_in_recovery()`): its usage counters count only the reads
    /// run on it.
    pub standby: bool,
    /// The login can do more than read statistics (`crate::trust::elevated_sql`): it doesn't plan while another role's code
    /// is in reach.
    pub elevated: bool,
}

pub enum StatementsAccess {
    Usable { relation: String, info: Option<String> },
    NotInstalled,
    NotLoaded(String),
    Failed(String),
}

impl Probe {
    pub fn product(&self) -> String {
        let short = self.version.split_whitespace().next().unwrap_or(&self.version);
        format!("PostgreSQL {short}")
    }

    pub fn generic_plans(&self) -> bool {
        self.version_num >= crate::generic::MIN_VERSION
    }

    fn statements_usable(&self) -> bool {
        matches!(self.statements, StatementsAccess::Usable { .. })
    }

    pub fn server_info(&self) -> ServerInfo {
        let mut capabilities = vec![
            Capability::TableStats,
            Capability::IndexUsage,
            Capability::Statistics,
            Capability::Sessions,
            Capability::Showplan,
        ];
        let mut missing = Vec::new();
        match &self.statements {
            StatementsAccess::Usable { .. } => capabilities.push(Capability::PgStatStatements),
            StatementsAccess::NotInstalled => missing.push(MissingCapability {
                capability: Capability::PgStatStatements,
                reason: "The pg_stat_statements extension is not installed in this database, so Culprit cannot see which queries use the most time.".into(),
                fix: Some(self.statements_fix()),
            }),
            StatementsAccess::NotLoaded(message) => missing.push(MissingCapability {
                capability: Capability::PgStatStatements,
                reason: format!("pg_stat_statements is installed but not loaded by the server ({message})."),
                fix: Some(self.statements_fix()),
            }),
            StatementsAccess::Failed(message) => missing.push(MissingCapability {
                capability: Capability::PgStatStatements,
                reason: format!("Reading pg_stat_statements failed: {message}"),
                fix: Some(format!("GRANT pg_read_all_stats TO {};", quote_ident(&self.user))),
            }),
        }
        if self.generic_plans() {
            capabilities.push(Capability::GenericPlans);
            if self.statements_usable() {
                capabilities.push(Capability::QueryPlans);
            } else {
                missing.push(MissingCapability {
                    capability: Capability::QueryPlans,
                    reason: "Plans for the heaviest queries come from pg_stat_statements, which is not available."
                        .into(),
                    fix: Some(self.statements_fix()),
                });
            }
        } else {
            let reason = format!(
                "Planning the heaviest queries without running them needs PostgreSQL 12 or newer. This server runs {}.",
                self.version.split_whitespace().next().unwrap_or(&self.version)
            );
            let fix = Some(
                "Upgrade to PostgreSQL 12 or newer, or explain a query with sample values in the editor.".to_string(),
            );
            missing.push(MissingCapability {
                capability: Capability::GenericPlans,
                reason: reason.clone(),
                fix: fix.clone(),
            });
            missing.push(MissingCapability { capability: Capability::QueryPlans, reason, fix });
        }
        if self.privileged {
            capabilities.push(Capability::WaitStats);
        } else {
            missing.push(MissingCapability {
                capability: Capability::WaitStats,
                reason: "Other sessions' state and waits are hidden from this login.".into(),
                fix: Some(format!("GRANT pg_monitor TO {};", quote_ident(&self.user))),
            });
        }
        ServerInfo {
            engine: Engine::Postgres,
            product: self.product(),
            version: self.version.clone(),
            edition: None,
            cloud: self.cloud.clone(),
            database: self.database.clone(),
            database_size_bytes: self.size_bytes,
            started_at: self.started_at.clone(),
            read_only_session: true,
            capabilities,
            missing,
        }
    }

    fn statements_fix(&self) -> String {
        if self.preloaded {
            let who = if self.can_create_extension {
                String::new()
            } else {
                " as a user who can create extensions (an admin role, or the database owner)".to_string()
            };
            return format!(
                "Run CREATE EXTENSION pg_stat_statements; in this database{who}. The server already loads it, so no restart is \
                 needed and the statistics it has been recording since the last restart appear right away."
            );
        }
        match self.cloud.as_deref() {
            Some("Amazon RDS" | "Amazon Aurora") => "Add pg_stat_statements to shared_preload_libraries in the DB parameter group, reboot, then run CREATE EXTENSION pg_stat_statements; in this database.".into(),
            Some("Azure Database for PostgreSQL") => "Add PG_STAT_STATEMENTS to shared_preload_libraries and azure.extensions in the server parameters, restart, then run CREATE EXTENSION pg_stat_statements; in this database.".into(),
            Some("Google Cloud SQL") => "Run CREATE EXTENSION pg_stat_statements; in this database (Cloud SQL preloads it).".into(),
            _ => PG_STAT_STATEMENTS_FIX.into(),
        }
    }
}

pub async fn probe(client: &Client, spec_host: &str) -> Result<Probe> {
    let sql = format!(
        "SELECT current_setting('server_version_num') AS version_num,
                current_setting('server_version') AS version,
                current_database() AS database,
                pg_database_size(current_database()) AS size_bytes,
                {started} AS started_at,
                {now} AS now,
                current_user AS username,
                (SELECT rolsuper FROM pg_roles WHERE rolname = current_user)
                  OR pg_has_role(current_user, 'pg_read_all_stats', 'MEMBER') AS privileged,
                (SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
                  WHERE e.extname = 'pg_stat_statements') AS pgss_schema,
                (SELECT string_agg(DISTINCT split_part(name, '.', 1), ',') FROM pg_settings
                  WHERE name ~ '^(rds|azure|cloudsql|alloydb|neon)\\.') AS cloud_settings,
                EXISTS (SELECT 1 FROM pg_proc WHERE proname = 'aurora_version') AS aurora,
                'pg_stat_statements' = ANY (string_to_array(replace(current_setting('shared_preload_libraries'), ' ', ''), ',')) AS preloaded,
                (SELECT rolsuper FROM pg_roles WHERE rolname = current_user)
                  OR EXISTS (SELECT 1 FROM pg_roles r WHERE r.rolname IN ('rds_superuser', 'azure_pg_admin', 'cloudsqlsuperuser')
                               AND pg_has_role(current_user, r.oid, 'MEMBER'))
                  OR has_database_privilege(current_database(), 'CREATE') AS can_create_extension,
                pg_is_in_recovery() AS in_recovery,
                {elevated} AS elevated",
        started = iso("pg_postmaster_start_time()"),
        elevated = crate::trust::elevated_sql(),
        now = iso("now()"),
    );
    let row = query_one(client, &sql).await?.ok_or_else(|| culprit_core::Error::internal("Probe returned nothing"))?;
    let version_num = row.number("version_num").unwrap_or(0.0) as u32;
    let cloud = detect_cloud(&row.string("cloud_settings"), row.flag("aurora"), spec_host);
    let mut probe = Probe {
        version_num,
        version: row.string("version"),
        database: row.string("database"),
        size_bytes: row.number("size_bytes"),
        started_at: row.text("started_at"),
        now: row.string("now"),
        user: row.string("username"),
        privileged: row.flag("privileged"),
        cloud,
        statements: StatementsAccess::NotInstalled,
        preloaded: row.flag("preloaded"),
        can_create_extension: row.flag("can_create_extension"),
        standby: row.flag("in_recovery"),
        elevated: row.flag("elevated"),
    };
    if let Some(schema) = row.text("pgss_schema") {
        probe.statements = statements_access(client, &schema, version_num).await;
    }
    Ok(probe)
}

async fn statements_access(client: &Client, schema: &str, version_num: u32) -> StatementsAccess {
    let relation = format!("{}.pg_stat_statements", quote_ident(schema));
    if let Err(e) = client.simple_query(&crate::rows::tagged(&format!("SELECT 1 FROM {relation} LIMIT 1"))).await {
        let message = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string());
        return match sql_state(&e) {
            Some("55000") => StatementsAccess::NotLoaded(message),
            _ => StatementsAccess::Failed(message),
        };
    }
    let mut info = None;
    if version_num >= 140_000 {
        let sql =
            format!("SELECT {} AS reset FROM {}.pg_stat_statements_info", iso("stats_reset"), quote_ident(schema));
        if let Ok(Some(row)) = query_one(client, &sql).await {
            info = row.text("reset");
        }
    }
    StatementsAccess::Usable { relation, info }
}

fn detect_cloud(settings: &str, aurora: bool, host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let has = |prefix: &str| settings.split(',').any(|s| s == prefix);
    let name = if aurora {
        "Amazon Aurora"
    } else if has("rds") || host.ends_with(".rds.amazonaws.com") {
        "Amazon RDS"
    } else if has("azure") || host.ends_with(".postgres.database.azure.com") {
        "Azure Database for PostgreSQL"
    } else if has("alloydb") {
        "Google AlloyDB"
    } else if has("cloudsql") {
        "Google Cloud SQL"
    } else if has("neon") || host.ends_with(".neon.tech") {
        "Neon"
    } else if host.contains("supabase.co") || host.contains("supabase.com") {
        "Supabase"
    } else {
        return None;
    };
    Some(name.to_string())
}

pub fn quote_ident(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        return name.to_string();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Collect a full snapshot. Each collector that fails becomes a note; only an unreachable server is an error.
pub async fn snapshot(
    connector: &Connector,
    spec: &ConnectionSpec,
    connection: &str,
    options: &SnapshotOptions,
) -> Result<Snapshot> {
    let started = Instant::now();
    let primary = connector.connect(COLLECTOR).await?;
    let probe = probe(&primary, &connector.host).await?;

    let (workload, schema, misc) = tokio::join!(
        collect_workload(&primary, &probe, options),
        async {
            let client = connector.connect(COLLECTOR).await?;
            Ok::<_, culprit_core::Error>((
                tables(&client, probe.version_num).await,
                indexes(&client, &probe).await,
                foreign_keys(&client).await,
                columns(&client).await,
            ))
        },
        async {
            let client = connector.connect(COLLECTOR).await?;
            Ok::<_, culprit_core::Error>((settings(&client).await, crate::pressure::collect(&client, &probe).await))
        },
    );

    let mut server = probe.server_info();
    // What the sources could read, starting from what the probe found (`culprit_core::Coverage`).
    let mut coverage = Coverage::with(std::mem::take(&mut server.capabilities), std::mem::take(&mut server.missing));

    let mut families = Vec::new();
    let mut statement_status = Vec::new();
    let (statements_ms, statements_io_ms) = match &workload {
        Ok(Some(c)) => (c.workload.total_time_ms, c.io_ms),
        _ => (0.0, 0.0),
    };
    let workload = match workload {
        Ok(Some(collected)) => {
            if let Some(message) = collected.plan_note {
                coverage.note("plans", message);
            }
            let withheld = collected.plans_withheld.is_some();
            if let Some(e) = collected.plans_withheld {
                coverage.lost(Capability::QueryPlans, e.message, e.hint);
            }
            families = collected.families;
            statement_status = collected.status;
            if !withheld && collected.workload.queries.iter().all(|q| q.plan.is_none()) {
                coverage.withdraw(Capability::QueryPlans);
            }
            Some(collected.workload)
        }
        Ok(None) => None,
        Err(e) => {
            coverage.note("workload", e.message.clone());
            coverage.lost(
                Capability::PgStatStatements,
                format!("Reading pg_stat_statements failed: {}", e.message),
                e.hint,
            );
            None
        }
    };
    if !probe.privileged && probe.statements_usable() {
        coverage.note(
            "workload",
            format!(
                "Statements run by other roles show as <insufficient privilege>. GRANT pg_read_all_stats TO {}; to see them.",
                quote_ident(&probe.user)
            ),
        );
    }

    let (mut tables_out, mut indexes_out, mut fks_out, mut columns_out) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    match schema {
        Ok((tables, indexes, fks, cols)) => {
            match cols {
                Ok(c) => columns_out = c,
                Err(e) => coverage.note("columns", e.message),
            }
            match tables {
                Ok(t) => tables_out = t,
                Err(e) => {
                    coverage.note("tables", e.message.clone());
                    coverage.lost(
                        Capability::TableStats,
                        format!("Reading table statistics failed: {}", e.message),
                        e.hint,
                    );
                    coverage.lost(Capability::Statistics, "Table statistics are unavailable.", None);
                }
            }
            match indexes {
                Ok(i) => indexes_out = i,
                Err(e) => {
                    coverage.note("indexes", e.message.clone());
                    coverage.lost(
                        Capability::IndexUsage,
                        format!("Reading index statistics failed: {}", e.message),
                        e.hint,
                    );
                }
            }
            match fks {
                Ok(f) => fks_out = f,
                Err(e) => coverage.note("foreign_keys", e.message),
            }
        }
        Err(e) => {
            coverage.note("schema", e.message.clone());
            coverage.lost(Capability::TableStats, e.message.clone(), e.hint.clone());
            coverage.lost(Capability::IndexUsage, e.message.clone(), e.hint.clone());
            coverage.lost(Capability::Statistics, e.message, e.hint);
        }
    }

    let mut settings_out = Vec::new();
    let mut pressure_out = None;
    match misc {
        Ok((settings, (pressure, pressure_notes))) => {
            match settings {
                Ok(s) => settings_out = s,
                Err(e) => coverage.note("settings", e.message),
            }
            for message in pressure_notes {
                coverage.note("pressure", message);
            }
            pressure_out = Some(pressure);
        }
        Err(e) => coverage.note("settings", e.message),
    }
    settings_out.extend(statement_status);
    // A standby's usage counters are its own reads, next to the primary's statement history: say what it is, and the
    // engine sets its usage counters aside.
    if probe.standby {
        settings_out.push(Setting {
            name: culprit_core::REPLICA_SETTING.into(),
            value: "standby".into(),
            default_value: None,
            scope: "server".into(),
        });
    }
    // PostgreSQL keeps no wait history: time by category comes from statement and WAL timing, when they are on.
    let waits_out = crate::pressure::history_waits(&primary, &probe, statements_ms, statements_io_ms).await;
    if waits_out.is_empty() {
        coverage.withdraw(Capability::WaitStats);
    } else {
        coverage.delivered(Capability::WaitStats);
    }
    let (have, missing, notes) = coverage.finish();
    server.capabilities = have;
    server.missing = missing;

    let statistics = tables_out
        .iter()
        .map(|t| StatisticsInfo {
            schema: t.schema.clone(),
            table: t.name.clone(),
            name: t.name.clone(),
            last_updated: t.last_analyze.clone(),
            rows: t.rows,
            rows_sampled: None,
            modifications: t.modifications_since_analyze,
            auto_created: false,
        })
        .collect();

    let mut snapshot = Snapshot {
        format: SNAPSHOT_FORMAT,
        id: probe.now.replace(['-', ':'], ""),
        connection: connection.to_string(),
        environment: spec.environment,
        taken_at: probe.now.clone(),
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        server,
        workload,
        tables: tables_out,
        indexes: indexes_out,
        columns: columns_out,
        foreign_keys: fks_out,
        missing_indexes: Vec::new(),
        statistics,
        waits: waits_out,
        resources: Vec::new(),
        settings: settings_out,
        pressure: pressure_out,
        notes,
        options: Some(options.clone()),
        query_families: families,
        plan_cache: None,
        tuning: None,
        deadlocks: None,
        constraints: Vec::new(),
        histograms: None,
        escalations_before: None,
        sizes_before: None,
    };
    crate::signals::collect(&primary, &probe, &mut snapshot).await;
    snapshot.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(snapshot)
}

struct CollectedWorkload {
    workload: Workload,
    plan_note: Option<String>,
    /// Why no statement was planned, when the login may not plan here (`crate::trust`).
    plans_withheld: Option<culprit_core::Error>,
    families: Vec<QueryFamily>,
    /// pg_stat_statements' own counters (entries evicted since reset), surfaced as status settings.
    status: Vec<Setting>,
    /// Disk read/write time across all statements (zero unless track_io_timing is on).
    io_ms: f64,
}

async fn collect_workload(
    client: &Client,
    probe: &Probe,
    options: &SnapshotOptions,
) -> Result<Option<CollectedWorkload>> {
    let StatementsAccess::Usable { relation, info } = &probe.statements else {
        return Ok(None);
    };
    let base = statements_base(relation, probe.version_num);
    match collect_with_base(client, probe, options, &base, info.as_deref()).await {
        // An extension left on its pre-13 column names after pg_upgrade.
        Err(e) if e.code == culprit_core::ErrorCode::InvalidInput && probe.version_num >= 130_000 => {
            collect_with_base(client, probe, options, &statements_base_legacy(relation), info.as_deref()).await
        }
        other => other,
    }
}

async fn collect_with_base(
    client: &Client,
    probe: &Probe,
    options: &SnapshotOptions,
    base: &str,
    reset: Option<&str>,
) -> Result<Option<CollectedWorkload>> {
    let totals = query_one(
        client,
        &format!("SELECT coalesce(sum(total_ms), 0) AS total_ms, coalesce(sum(calls), 0) AS calls, count(*) AS n, coalesce(sum(io_ms), 0) AS io_ms FROM ({base}) s"),
    )
    .await?;
    let (total_ms, total_calls, count, io_ms) = totals
        .map(|r| {
            (
                r.number("total_ms").unwrap_or(0.0),
                r.number("calls").unwrap_or(0.0),
                r.number("n").unwrap_or(0.0),
                r.number("io_ms").unwrap_or(0.0),
            )
        })
        .unwrap_or_default();

    let (window_start, window_end, window_hours) = window(client, reset).await?;

    let n = options.top_queries.max(1) as f64;
    let by_time = (n * 0.6).ceil() as u32;
    let by_other = (n * 0.2).ceil() as u32;
    let top_sql = format!(
        "WITH s AS ({base}),
         r AS (SELECT s.*,
                 row_number() OVER (ORDER BY total_ms DESC) AS by_time,
                 row_number() OVER (ORDER BY calls DESC) AS by_calls,
                 row_number() OVER (ORDER BY reads DESC) AS by_reads
               FROM s WHERE id IS NOT NULL AND query <> '<insufficient privilege>')
         SELECT * FROM r WHERE by_time <= {by_time} OR by_calls <= {by_other} OR by_reads <= {by_other}
         ORDER BY total_ms DESC LIMIT {limit}",
        limit = options.top_queries.max(1)
    );
    let rows = query(client, &top_sql).await?;
    let mut queries: Vec<QueryStat> = rows
        .iter()
        .map(|r| QueryStat {
            id: r.string("id"),
            query_hash: None,
            text: r.string("query"),
            parameters: None,
            object_name: None,
            executions: r.number("calls").unwrap_or(0.0),
            total_time_ms: r.number("total_ms").unwrap_or(0.0),
            mean_time_ms: r.number("mean_ms").unwrap_or(0.0),
            max_time_ms: r.number("max_ms"),
            stddev_time_ms: r.number("stddev_ms"),
            cpu_time_ms: None,
            rows: r.number("rows"),
            logical_reads: r.number("reads"),
            physical_reads: r.number("phys"),
            temp_writes: r.number("temp"),
            first_seen: r.text("first_seen"),
            last_seen: None,
            plans: Vec::new(),
            plan: None,
            // Disk time is the only part of a statement's time PostgreSQL measures per statement.
            waits: r
                .number("io_ms")
                .filter(|ms| *ms > 0.0)
                .map(|ms| {
                    vec![WaitStat {
                        category: "IO".into(),
                        wait_type: Some("data file reads and writes".into()),
                        wait_ms: ms,
                        tasks: None,
                    }]
                })
                .unwrap_or_default(),
            trend: None,
            spread: r.number("min_ms").map(|min_ms| Spread { min_ms: Some(min_ms), ..Default::default() }),
            costs: None,
            // PostgreSQL's `rows` counts rows returned and rows changed alike.
            rows_affected: None,
            plan_id: None,
        })
        .collect();
    // The text of each statement as this login ran it, when it did: the only text an elevated login plans, so what is
    // planned is what passed that check, never another role's copy of the same statement.
    let own: std::collections::HashMap<String, String> =
        rows.iter().filter_map(|r| Some((r.string("id"), r.text("own_query")?))).collect();
    let (plan_note, plans_withheld) = if probe.generic_plans() {
        match attach_plans(client, &mut queries, &own, probe, options.plans).await {
            Ok(note) => (note, None),
            Err(withheld) => (None, Some(withheld)),
        }
    } else {
        (None, None)
    };
    // Families and eviction counts are extras: a failure leaves them out, never the workload.
    let families = query_families(client, base).await.unwrap_or_default();
    let mut status = Vec::new();
    if reset.is_some()
        && let Some(info) = base_relation(base).map(|r| format!("{r}_info"))
        && let Ok(Some(row)) = query_one(client, &format!("SELECT dealloc FROM {info}")).await
        && let Some(dealloc) = row.number("dealloc")
    {
        status.push(Setting {
            name: "pg_stat_statements.dealloc".into(),
            value: format!("{dealloc:.0}"),
            default_value: None,
            scope: "status".into(),
        });
    }

    Ok(Some(CollectedWorkload {
        workload: Workload {
            source: "pg_stat_statements".into(),
            windowed: false,
            window_start,
            window_end,
            window_hours,
            total_time_ms: total_ms,
            total_cpu_ms: None,
            total_executions: total_calls,
            query_count: count,
            queries,
            timeline: None,
            untracked_time_ms: None,
            min_logged_ms: None,
        },
        plan_note,
        plans_withheld,
        families,
        status,
        io_ms,
    }))
}

/// When pg_stat_statements started counting. 14+ says (`pg_stat_statements_info.stats_reset`). Before that only a
/// server that discards its statistics at shutdown (`pg_stat_statements.save = off`) bounds it: they started at the
/// last start or later. With `save` on (the default) they outlive restarts, so the start time says nothing: the window
/// is unknown (`window_start` empty, 0 hours) and Culprit's earlier scans of the database narrow it down.
async fn window(client: &Client, reset: Option<&str>) -> Result<(Option<String>, Option<String>, f64)> {
    let start = match reset {
        Some(reset) => format!("'{reset}'::timestamptz"),
        None => {
            "CASE WHEN current_setting('pg_stat_statements.save', true) = 'off' THEN pg_postmaster_start_time() END"
                .to_string()
        }
    };
    let window_sql = format!(
        "SELECT {start_text} AS start, {now} AS end_at, extract(epoch FROM now() - ({start})) / 3600 AS hours",
        start_text = iso(&start),
        now = iso("now()")
    );
    let window = query_one(client, &window_sql).await?;
    Ok(window
        .map(|r| {
            let start = r.text("start");
            let hours = if start.is_some() { r.number("hours").unwrap_or(0.0) } else { 0.0 };
            (start, r.text("end_at"), hours)
        })
        .unwrap_or((None, None, 0.0)))
}

/// The pg_stat_statements relation a base query reads (`"public".pg_stat_statements`).
fn base_relation(base: &str) -> Option<&str> {
    let from = base.find(" FROM ")? + " FROM ".len();
    base[from..].split_whitespace().next()
}

/// Statements pg_stat_statements keeps apart only because a value list has a different length (`IN ($1, $2)` vs
/// `IN ($1, $2, $3)`) or a VALUES clause a different number of rows: each is planned and tracked on its own. A family
/// counts the texts that differ before their lists are collapsed: the same text under several statement ids (one per
/// schema a tenant's search_path resolves to) is one variant, not a value-list split.
async fn query_families(client: &Client, base: &str) -> Result<Vec<QueryFamily>> {
    Ok(query(client, &families_sql(base)).await?.iter().map(family_of).collect())
}

fn families_sql(base: &str) -> String {
    format!(
        r#"WITH s AS ({base}),
         t AS (
           SELECT id, calls, total_ms, query, regexp_replace(regexp_replace(query, '\$\d+', '$N', 'g'), '\s+', ' ', 'g') AS shape
           FROM s WHERE id IS NOT NULL AND query <> '<insufficient privilege>'),
         n AS (
           SELECT id, calls, total_ms, query, shape,
             regexp_replace(regexp_replace(shape, '\$N(\s*,\s*\$N)+', '$N', 'g'), '(\(\$N\)\s*,\s*)+\(\$N\)', '($N), …', 'g') AS family
           FROM t)
         SELECT md5(family) AS key, count(DISTINCT shape) AS variants, sum(calls) AS executions, sum(total_ms) AS total_ms,
                (array_agg(query ORDER BY total_ms DESC))[1] AS sample,
                array_to_string((array_agg(id::text ORDER BY total_ms DESC))[1:5], ',') AS ids
         FROM n GROUP BY family HAVING count(DISTINCT shape) >= 5
         ORDER BY count(DISTINCT shape) DESC LIMIT 15"#
    )
}

fn family_of(r: &crate::rows::Row) -> QueryFamily {
    QueryFamily {
        key: r.string("key"),
        sample: r.string("sample"),
        variants: r.number("variants").unwrap_or(0.0),
        executions: r.number("executions").unwrap_or(0.0),
        total_time_ms: r.number("total_ms").unwrap_or(0.0),
        cached_bytes: None,
        query_ids: r.string("ids").split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
    }
}

/// What one pg_stat_statements version calls each column, per entry.
struct EntryColumns {
    total: &'static str,
    mean: &'static str,
    max: &'static str,
    stddev: &'static str,
    min: &'static str,
    io: &'static str,
    /// `stats_since` (17+): when the entry started counting.
    since: Option<&'static str>,
    toplevel: bool,
}

/// One row per statement: pg_stat_statements keeps an entry per (role, database, queryid), so a statement several roles
/// run (an app's pool and its migrations, one login per tenant) is several entries splitting its time. Their counts and
/// times add up; the slowest and fastest run are the extremes over the entries; the spread is pooled from each entry's
/// mean and standard deviation (`Σn(σ²+μ²)/N − μ²`). The text and owner are the busiest entry's.
fn aggregated_base(relation: &str, c: &EntryColumns) -> String {
    let toplevel = if c.toplevel { " AND toplevel" } else { "" };
    let first_seen = c.since.map(|since| iso(&format!("min({since})"))).unwrap_or_else(|| "NULL::text".to_string());
    format!(
        "SELECT queryid::text AS id, (array_agg(query ORDER BY {total} DESC))[1] AS query, sum(calls) AS calls,
                sum({total}) AS total_ms, sum({total}) / nullif(sum(calls), 0)::float8 AS mean_ms,
                max({max}) AS max_ms, min({min}) AS min_ms,
                sqrt(greatest(sum(calls * ({stddev} ^ 2 + {mean} ^ 2)) / nullif(sum(calls), 0)::float8
                              - (sum({total}) / nullif(sum(calls), 0)::float8) ^ 2, 0)) AS stddev_ms,
                sum(rows) AS rows, sum(shared_blks_hit + shared_blks_read) AS reads, sum(shared_blks_read) AS phys,
                sum(temp_blks_written) AS temp, sum({io}) AS io_ms, {first_seen} AS first_seen,
                count(*) AS entries,
                (array_agg(query ORDER BY {total} DESC) FILTER (WHERE pg_get_userbyid(userid) = current_user))[1] AS own_query
         FROM {relation}
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()){toplevel}
           AND query NOT LIKE '%{TAG}'
         GROUP BY queryid",
        total = c.total,
        mean = c.mean,
        max = c.max,
        stddev = c.stddev,
        min = c.min,
        io = c.io,
    )
}

fn statements_base(relation: &str, version_num: u32) -> String {
    let modern = version_num >= 130_000;
    aggregated_base(
        relation,
        &EntryColumns {
            total: if modern { "total_exec_time" } else { "total_time" },
            mean: if modern { "mean_exec_time" } else { "mean_time" },
            max: if modern { "max_exec_time" } else { "max_time" },
            stddev: if modern { "stddev_exec_time" } else { "stddev_time" },
            min: if modern { "min_exec_time" } else { "min_time" },
            io: crate::pressure::statements_io_column(version_num),
            since: (version_num >= 170_000).then_some("stats_since"),
            toplevel: version_num >= 140_000,
        },
    )
}

fn statements_base_legacy(relation: &str) -> String {
    aggregated_base(
        relation,
        &EntryColumns {
            total: "total_time",
            mean: "mean_time",
            max: "max_time",
            stddev: "stddev_time",
            min: "min_time",
            io: "(blk_read_time + blk_write_time)",
            since: None,
            toplevel: false,
        },
    )
}

/// Attach generic plans to the heaviest plannable statements. Returns a note when some could not be planned, or why
/// this login plans none here.
async fn attach_plans(
    client: &Client,
    queries: &mut [QueryStat],
    own: &std::collections::HashMap<String, String>,
    probe: &Probe,
    budget: u32,
) -> Result<Option<String>> {
    // Planning runs the code a statement reaches with this login's privileges (`crate::trust`). A login that can do
    // more than read statistics plans only statements it ran itself, and none while another role's code is in reach;
    // each plan checks again, in its own read-only transaction. Statements are planned under Culprit's own
    // search_path, never the role's that ran them: a statement whose names don't resolve here stays unplanned.
    if probe.elevated {
        let owners = crate::trust::foreign_code(client).await?;
        if !owners.is_empty() {
            return Err(crate::trust::withheld(&owners));
        }
        // Before 14 pg_stat_statements can't tell a statement the login ran from one run inside a function on its
        // behalf (no toplevel): with track = all, another role's trigger code is recorded as this login's own.
        if probe.version_num < 140_000 {
            let track =
                query_one(client, "SELECT pg_catalog.current_setting('pg_stat_statements.track', true) AS track")
                    .await?
                    .and_then(|r| r.text("track"))
                    .unwrap_or_default();
            if track.eq_ignore_ascii_case("all") {
                return Err(culprit_core::Error::new(
                    culprit_core::ErrorCode::PermissionDenied,
                    "Not planned: before PostgreSQL 14, with pg_stat_statements.track = all, statements other roles' functions run on this login's behalf are recorded as its own, and this login can do more than read statistics.",
                )
                .with_hint("Set pg_stat_statements.track = top, or connect with a login that has only pg_monitor, and SELECT on the tables."));
            }
        }
    }
    let mut order: Vec<usize> = (0..queries.len()).collect();
    order.sort_by(|a, b| queries[*b].total_time_ms.total_cmp(&queries[*a].total_time_ms));
    let (mut attempted, mut planned, mut others) = (0u32, 0u32, 0u32);
    for index in order {
        if attempted >= budget {
            break;
        }
        let text = if probe.elevated {
            match own.get(&queries[index].id) {
                Some(text) => text.as_str(),
                None => {
                    others += plannable(&queries[index].text).is_some() as u32;
                    continue;
                }
            }
        } else {
            queries[index].text.as_str()
        };
        let Some(statement) = plannable(text) else { continue };
        attempted += 1;
        let Some(json) = generic_plan_json(client, statement, probe).await else { continue };
        if let Ok(mut plan) = plan_json::parse(&json, PlanKind::Generic, Some(statement)) {
            crate::tables::fill(client, &mut plan).await;
            mark_generic(&mut plan);
            queries[index].plan = Some(plan);
            planned += 1;
        }
    }
    let mut notes = Vec::new();
    if planned < attempted {
        notes.push(format!(
            "Planned {planned} of {attempted} heavy statements. The rest could not be planned: their recorded text is truncated, is not valid SQL once constants are replaced by placeholders, or uses objects this login cannot see."
        ));
    }
    if others > 0 {
        notes.push(format!(
            "{others} heavy statements other roles ran were left unplanned: this login can do more than read statistics, and planning a statement runs the functions it reaches with this login's privileges. A login with only pg_monitor, and SELECT on the tables, plans them."
        ));
    }
    Ok((!notes.is_empty()).then(|| notes.join(" ")))
}

/// Say on a generic plan what it can't know, so what reads it can weigh it: no parameter values (estimates for `$n`
/// use default selectivities), and partitions pruned only when it runs.
pub(crate) fn mark_generic(plan: &mut culprit_core::Plan) {
    plan.props.insert(
        "Generic Plan".into(),
        "Planned without parameter values: estimates for $n use default selectivities, and partitions are pruned only when it runs.".into(),
    );
}

async fn generic_plan_json(client: &Client, statement: &str, probe: &Probe) -> Option<String> {
    // Statement text was written by whoever ran it; it must lex as exactly one statement before it reaches EXPLAIN.
    let statement = culprit_core::sql::single_statement(statement, Engine::Postgres).ok()?;
    let (version_num, elevated) = (probe.version_num, probe.elevated);
    match crate::generic::plan_json(client, statement, version_num, elevated).await {
        Ok(json) => Some(json),
        Err(_) => {
            let rewritten = cast_typed_placeholders(statement)?;
            let rewritten = culprit_core::sql::single_statement(&rewritten, Engine::Postgres).ok()?;
            crate::generic::plan_json(client, rewritten, version_num, elevated).await.ok()
        }
    }
}

const TYPED_LITERAL_TYPES: &[&str] =
    &["interval", "timestamptz", "timestamp", "date", "time", "uuid", "jsonb", "json", "inet", "cidr", "numeric"];

/// pg_stat_statements turns `interval '7 days'` into `interval $1`, which is not valid SQL. Rewrite those as `$1::interval`.
pub fn cast_typed_placeholders(statement: &str) -> Option<String> {
    let lower = statement.to_ascii_lowercase();
    let bytes = statement.as_bytes();
    let mut out = String::with_capacity(statement.len() + 16);
    let mut i = 0;
    let mut changed = false;
    'scan: while i < bytes.len() {
        let at_word_start = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if at_word_start {
            for kind in TYPED_LITERAL_TYPES {
                if !lower[i..].starts_with(kind) {
                    continue;
                }
                let after = &statement[i + kind.len()..];
                let trimmed = after.trim_start();
                let gap = after.len() - trimmed.len();
                let digits: String = trimmed
                    .strip_prefix('$')
                    .map(|r| r.chars().take_while(char::is_ascii_digit).collect())
                    .unwrap_or_default();
                if gap == 0 || digits.is_empty() {
                    continue;
                }
                out.push_str(&format!("${digits}::{kind}"));
                i += kind.len() + gap + 1 + digits.len();
                changed = true;
                continue 'scan;
            }
        }
        let ch = statement[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    changed.then_some(out)
}

/// The statement text if it is a single SELECT / INSERT / UPDATE / DELETE / MERGE / WITH that EXPLAIN can plan.
pub fn plannable(text: &str) -> Option<&str> {
    let statement = text.trim().trim_end_matches(';').trim_end();
    if statement.contains(';') {
        return None;
    }
    let keyword = leading_keyword(statement)?;
    matches!(keyword.as_str(), "select" | "insert" | "update" | "delete" | "with" | "merge" | "values" | "table")
        .then_some(statement)
}

pub fn leading_keyword(text: &str) -> Option<String> {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix('(') {
            rest = after.trim_start();
        } else {
            break;
        }
    }
    let word: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

async fn tables(client: &Client, version_num: u32) -> Result<Vec<TableStat>> {
    let exact = "pg_total_relation_size(s.relid) AS total_bytes, pg_table_size(s.relid) AS data_bytes, pg_indexes_size(s.relid) AS index_bytes,
                 pg_relation_size(s.relid) AS in_row_bytes,
                 CASE WHEN c.reltoastrelid = 0 THEN 0 ELSE pg_total_relation_size(c.reltoastrelid) END AS lob_bytes";
    let estimated = "(c.relpages::bigint + coalesce((SELECT sum(i.relpages) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = s.relid), 0)) * current_setting('block_size')::bigint AS total_bytes,
                     c.relpages::bigint * current_setting('block_size')::bigint AS data_bytes,
                     coalesce((SELECT sum(i.relpages) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = s.relid), 0) * current_setting('block_size')::bigint AS index_bytes,
                     c.relpages::bigint * current_setting('block_size')::bigint AS in_row_bytes,
                     coalesce((SELECT t.relpages FROM pg_class t WHERE t.oid = c.reltoastrelid), 0)::bigint * current_setting('block_size')::bigint AS lob_bytes";
    let sql = |sizes: &str| {
        format!(
            "SELECT s.schemaname AS schema_name, s.relname AS table_name,
                    {rows} AS rows,
                    {sizes},
                    EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = s.relid AND k.contype = 'p') AS has_pk,
                    s.seq_scan, s.seq_tup_read, s.idx_scan, s.n_tup_ins, s.n_tup_upd, s.n_tup_del, s.n_dead_tup,
                    s.n_mod_since_analyze,
                    {vacuum} AS last_vacuum,
                    {analyze} AS last_analyze,
                    CASE WHEN c.relkind = 'p' THEN NULL ELSE age(c.relfrozenxid) END AS xid_age,
                    {partition_of} AS partition_of
             FROM pg_stat_user_tables s JOIN pg_class c ON c.oid = s.relid",
            rows = ROWS_SQL,
            vacuum = iso("greatest(s.last_vacuum, s.last_autovacuum)"),
            analyze = iso("greatest(s.last_analyze, s.last_autoanalyze)"),
            partition_of = if version_num >= 100_000 { partition_parent("c") } else { "NULL::text".to_string() },
        )
    };
    let rows = match query(client, &sql(exact)).await {
        Ok(rows) => rows,
        Err(e) if e.code == culprit_core::ErrorCode::Timeout => query(client, &sql(estimated)).await?,
        Err(e) => return Err(e),
    };
    Ok(rows
        .iter()
        .map(|r| TableStat {
            schema: r.string("schema_name"),
            name: r.string("table_name"),
            rows: r.number("rows").map(|v| v.max(0.0)),
            total_bytes: r.number("total_bytes"),
            data_bytes: r.number("data_bytes"),
            index_bytes: r.number("index_bytes"),
            has_primary_key: r.flag("has_pk"),
            is_heap: None,
            seq_scans: r.number("seq_scan"),
            seq_rows_read: r.number("seq_tup_read"),
            index_scans: r.number("idx_scan"),
            inserts: r.number("n_tup_ins"),
            updates: r.number("n_tup_upd"),
            deletes: r.number("n_tup_del"),
            dead_rows: r.number("n_dead_tup"),
            modifications_since_analyze: r.number("n_mod_since_analyze"),
            last_vacuum: r.text("last_vacuum"),
            last_analyze: r.text("last_analyze"),
            forwarded_records: None,
            forwarded_fetches: None,
            xid_age: r.number("xid_age"),
            storage: None,
            in_row_bytes: r.number("in_row_bytes"),
            lob_bytes: r.number("lob_bytes"),
            overflow_bytes: None,
            partition_of: r.text("partition_of"),
            counters_since: None,
        })
        .collect())
}

/// A table's row count. `n_live_tup` is a statistics counter: after a reset (`pg_stat_reset()`, crash recovery, a
/// restore, a promoted standby) it counts only inserts minus deletes since, so a 10-million-row table reads as a few
/// thousand rows until a vacuum or analyze sets it again. Until one has (no vacuum or analyze time since the reset),
/// the planner's own count (`reltuples`, kept in the catalog) is the better one; a table never analyzed has only the
/// counter. Partitioned parents hold no rows of their own.
const ROWS_SQL: &str = "CASE WHEN c.relkind = 'p' THEN NULL
                             WHEN c.reltuples < 0 THEN s.n_live_tup
                             WHEN s.last_vacuum IS NULL AND s.last_autovacuum IS NULL AND s.last_analyze IS NULL
                                  AND s.last_autoanalyze IS NULL THEN greatest(c.reltuples, s.n_live_tup)
                             ELSE s.n_live_tup END";

/// The relation `alias` (a pg_class row) is a partition of, as `schema.name` (PostgreSQL 10+ for tables, 11+ for
/// indexes); NULL for one that isn't.
fn partition_parent(alias: &str) -> String {
    format!(
        "CASE WHEN {alias}.relispartition THEN (SELECT pn.nspname || '.' || pc.relname FROM pg_inherits ih
           JOIN pg_class pc ON pc.oid = ih.inhparent JOIN pg_namespace pn ON pn.oid = pc.relnamespace
          WHERE ih.inhrelid = {alias}.oid LIMIT 1) END"
    )
}

async fn indexes(client: &Client, probe: &Probe) -> Result<Vec<IndexStat>> {
    let last_read = if probe.version_num >= 160_000 { iso("s.last_idx_scan") } else { "NULL::text".to_string() };
    let sql = format!(
        "SELECT n.nspname AS schema_name, t.relname AS table_name, i.relname AS index_name, am.amname AS kind,
                x.indisunique, x.indisprimary, x.indisvalid AND x.indisready AS usable,
                (SELECT string_agg(
                    CASE WHEN x.indkey[k - 1] <> 0
                         THEN (SELECT a.attname FROM pg_attribute a WHERE a.attrelid = x.indrelid AND a.attnum = x.indkey[k - 1])
                         ELSE pg_get_indexdef(x.indexrelid, k, true) END
                    || CASE WHEN (x.indoption[k - 1] & 1) = 1 THEN ' DESC' ELSE '' END,
                    chr(31) ORDER BY k)
                 FROM generate_series(1, x.indnkeyatts) AS k) AS key_columns,
                (SELECT string_agg(
                    CASE WHEN x.indkey[k - 1] <> 0
                         THEN (SELECT a.attname FROM pg_attribute a WHERE a.attrelid = x.indrelid AND a.attnum = x.indkey[k - 1])
                         ELSE pg_get_indexdef(x.indexrelid, k, true) END,
                    chr(31) ORDER BY k)
                 FROM generate_series(x.indnkeyatts + 1, x.indnatts) AS k) AS include_columns,
                (SELECT string_agg(concat_ws(' ',
                    CASE WHEN NOT opc.opcdefault THEN opc.opcname END,
                    CASE WHEN x.indcollation[k - 1] <> 0
                          AND x.indcollation[k - 1] IS DISTINCT FROM coalesce(
                              (SELECT a.attcollation FROM pg_attribute a WHERE a.attrelid = x.indrelid AND a.attnum = x.indkey[k - 1]),
                              100)
                         THEN 'COLLATE ' || quote_ident(coll.collname) END), chr(31) ORDER BY k)
                 FROM generate_series(1, x.indnkeyatts) AS k
                 JOIN pg_opclass opc ON opc.oid = x.indclass[k - 1]
                 LEFT JOIN pg_collation coll ON coll.oid = x.indcollation[k - 1]) AS key_opclasses,
                {partition_of} AS partition_of,
                pg_get_expr(x.indpred, x.indrelid, true) AS filter,
                i.relpages::bigint * current_setting('block_size')::bigint AS estimated_bytes,
                s.idx_scan, {last_read} AS last_read,
                pg_get_indexdef(x.indexrelid) AS definition
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         JOIN pg_class t ON t.oid = x.indrelid
         JOIN pg_namespace n ON n.oid = t.relnamespace
         JOIN pg_am am ON am.oid = i.relam
         LEFT JOIN pg_stat_user_indexes s ON s.indexrelid = x.indexrelid
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%'
           AND t.relkind IN ('r', 'm', 'p')",
        partition_of = partition_parent("i"),
    );
    let rows = query(client, &sql).await?;
    let mut indexes: Vec<IndexStat> = rows
        .iter()
        .map(|r| IndexStat {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("index_name"),
            kind: r.string("kind"),
            columns: r.list("key_columns"),
            include: r.list("include_columns"),
            filter: r.text("filter"),
            is_unique: r.flag("indisunique"),
            is_primary_key: r.flag("indisprimary"),
            is_clustered: false,
            is_disabled: !r.flag("usable"),
            size_bytes: r.number("estimated_bytes"),
            reads: r.number("idx_scan"),
            writes: None,
            seeks: None,
            scans: r.number("idx_scan"),
            lookups: None,
            last_read: r.text("last_read"),
            definition: r.text("definition"),
            contention: None,
            created_at_latest: None,
            partition_of: r.text("partition_of"),
            opclasses: opclasses(r.text("key_opclasses").as_deref()),
            counters_since: None,
        })
        .collect();

    // Exact sizes take a lock per index; fall back to the page-count estimate if any is held up.
    let sizes = query(
        client,
        "SELECT n.nspname AS schema_name, i.relname AS index_name, pg_relation_size(x.indexrelid) AS bytes
         FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_namespace n ON n.oid = i.relnamespace
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%'",
    )
    .await;
    if let Ok(sizes) = sizes {
        for row in sizes {
            let (schema, name) = (row.string("schema_name"), row.string("index_name"));
            if let Some(index) = indexes.iter_mut().find(|i| i.schema == schema && i.name == name) {
                index.size_bytes = row.number("bytes").or(index.size_bytes);
            }
        }
    }
    Ok(indexes)
}

/// Per key, its operator class and collation when they aren't the defaults (`key_opclasses`, chr(31)-separated, empty
/// for a key on the defaults). `None` when every key uses the defaults.
fn opclasses(list: Option<&str>) -> Option<Vec<String>> {
    let keys: Vec<String> = list?.split(crate::rows::LIST_SEPARATOR).map(|k| k.trim().to_string()).collect();
    keys.iter().any(|k| !k.is_empty()).then_some(keys)
}

async fn columns(client: &Client) -> Result<Vec<ColumnInfo>> {
    let sql = "SELECT n.nspname AS schema_name, c.relname AS table_name, a.attname AS column_name, a.attnum AS ordinal,
                      format_type(a.atttypid, a.atttypmod) AS data_type, NOT a.attnotnull AS nullable,
                      pg_get_expr(d.adbin, d.adrelid) AS default_value,
                      (a.attidentity <> '' OR a.attgenerated <> '' OR coalesce(pg_get_expr(d.adbin, d.adrelid), '') LIKE 'nextval(%') AS generated
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
               LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
               WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped
                 AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%'
               ORDER BY 1, 2, 4";
    let rows = query(client, sql).await?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("column_name"),
            data_type: r.string("data_type"),
            nullable: r.flag("nullable"),
            default: r.text("default_value"),
            generated: r.flag("generated"),
            ordinal: r.number("ordinal").unwrap_or(0.0) as u32,
        })
        .collect())
}

async fn foreign_keys(client: &Client) -> Result<Vec<ForeignKey>> {
    let sql = "SELECT c.conname, n.nspname AS schema_name, t.relname AS table_name,
                      rn.nspname AS ref_schema, rt.relname AS ref_table,
                      (SELECT string_agg(a.attname, chr(31) ORDER BY u.ord)
                         FROM unnest(c.conkey) WITH ORDINALITY AS u(attnum, ord)
                         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = u.attnum) AS columns,
                      (SELECT string_agg(a.attname, chr(31) ORDER BY u.ord)
                         FROM unnest(c.confkey) WITH ORDINALITY AS u(attnum, ord)
                         JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = u.attnum) AS ref_columns
               FROM pg_constraint c
               JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace
               JOIN pg_class rt ON rt.oid = c.confrelid JOIN pg_namespace rn ON rn.oid = rt.relnamespace
               WHERE c.contype = 'f' AND n.nspname NOT IN ('pg_catalog', 'information_schema')";
    // A key on a partitioned table is repeated for each partition (conparentid points at the original): keep the original.
    let rows = match query(client, &format!("{sql} AND c.conparentid = 0")).await {
        Ok(rows) => rows,
        Err(_) => query(client, sql).await?,
    };
    Ok(rows
        .iter()
        .map(|r| ForeignKey {
            name: r.string("conname"),
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            columns: r.list("columns"),
            ref_schema: r.string("ref_schema"),
            ref_table: r.string("ref_table"),
            ref_columns: r.list("ref_columns"),
        })
        .collect())
}

const SETTINGS: &[&str] = &[
    "pg_stat_statements.max",
    "pg_stat_statements.track",
    "pg_stat_statements.track_planning",
    "shared_buffers",
    "work_mem",
    "maintenance_work_mem",
    "effective_cache_size",
    "random_page_cost",
    "seq_page_cost",
    "effective_io_concurrency",
    "max_connections",
    "autovacuum",
    "autovacuum_vacuum_scale_factor",
    "autovacuum_analyze_scale_factor",
    "autovacuum_vacuum_threshold",
    "autovacuum_freeze_max_age",
    "jit",
    "jit_above_cost",
    "track_io_timing",
    "track_wal_io_timing",
    "default_statistics_target",
];

async fn settings(client: &Client) -> Result<Vec<Setting>> {
    let names = SETTINGS.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
    let rows = query(
        client,
        &format!("SELECT name, setting, unit, boot_val FROM pg_settings WHERE name IN ({names}) ORDER BY name"),
    )
    .await?;
    let mut out: Vec<Setting> = rows
        .iter()
        .map(|r| {
            let unit = r.text("unit");
            Setting {
                name: r.string("name"),
                value: pretty_setting(&r.string("setting"), unit.as_deref()),
                default_value: r.text("boot_val").map(|v| pretty_setting(&v, unit.as_deref())),
                scope: "server".into(),
            }
        })
        .collect();
    if let Some(row) =
        query_one(client, "SELECT age(datfrozenxid) AS age FROM pg_database WHERE datname = current_database()").await?
    {
        out.push(Setting {
            name: "datfrozenxid_age".into(),
            value: row.string("age"),
            default_value: None,
            scope: "database".into(),
        });
    }
    Ok(out)
}

/// Render a pg_settings value with its unit the way Postgres displays it (`4MB`, `200ms`).
pub fn pretty_setting(value: &str, unit: Option<&str>) -> String {
    let Some(unit) = unit else { return value.to_string() };
    let Ok(number) = value.trim().parse::<i64>() else { return value.to_string() };
    if number < 0 {
        return number.to_string();
    }
    const MEMORY: &[(&str, i64)] = &[("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10), ("B", 1)];
    const TIME: &[(&str, i64)] = &[("d", 86_400_000), ("h", 3_600_000), ("min", 60_000), ("s", 1_000), ("ms", 1)];
    let (base, scale) = match unit {
        "B" => (MEMORY, 1),
        "kB" => (MEMORY, 1 << 10),
        "8kB" => (MEMORY, 8 << 10),
        "16kB" => (MEMORY, 16 << 10),
        "32kB" => (MEMORY, 32 << 10),
        "MB" => (MEMORY, 1 << 20),
        "ms" => (TIME, 1),
        "s" => (TIME, 1_000),
        "min" => (TIME, 60_000),
        _ => return format!("{value} {unit}"),
    };
    let amount = number * scale;
    if amount == 0 {
        return format!("0{}", base.last().map(|(u, _)| *u).unwrap_or(""));
    }
    for (label, size) in base {
        if amount % size == 0 {
            return format!("{}{label}", amount / size);
        }
    }
    amount.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_render_like_postgres() {
        assert_eq!(pretty_setting("16384", Some("8kB")), "128MB");
        assert_eq!(pretty_setting("4096", Some("kB")), "4MB");
        assert_eq!(pretty_setting("200", Some("ms")), "200ms");
        assert_eq!(pretty_setting("60", Some("s")), "1min");
        assert_eq!(pretty_setting("4", None), "4");
        assert_eq!(pretty_setting("-1", Some("kB")), "-1");
    }

    #[test]
    fn plannable_statements() {
        assert_eq!(plannable("SELECT * FROM t WHERE id = $1;"), Some("SELECT * FROM t WHERE id = $1"));
        assert_eq!(plannable("/* app */ update t set a = $1"), Some("/* app */ update t set a = $1"));
        assert_eq!(
            plannable("-- hi\nWITH x AS (SELECT 1) SELECT * FROM x"),
            Some("-- hi\nWITH x AS (SELECT 1) SELECT * FROM x")
        );
        assert_eq!(plannable("BEGIN"), None);
        assert_eq!(plannable("SET work_mem = $1"), None);
        assert_eq!(plannable("SELECT 1; DROP TABLE t"), None);
        assert_eq!(plannable("(SELECT 1) UNION (SELECT 2)"), Some("(SELECT 1) UNION (SELECT 2)"));
    }

    #[test]
    fn typed_placeholders_become_casts() {
        assert_eq!(
            cast_typed_placeholders("SELECT 1 FROM t WHERE c > now() - interval $1 AND d = DATE $2").as_deref(),
            Some("SELECT 1 FROM t WHERE c > now() - $1::interval AND d = $2::date")
        );
        assert_eq!(cast_typed_placeholders("SELECT update_date FROM t WHERE id = $1"), None);
    }

    #[test]
    fn one_row_per_statement_whichever_roles_ran_it() {
        let base = statements_base("public.pg_stat_statements", 160_000);
        assert!(base.contains("GROUP BY queryid"), "{base}");
        assert!(base.contains("sum(calls) AS calls") && base.contains("sum(total_exec_time) AS total_ms"));
        assert!(base.contains("max(max_exec_time) AS max_ms") && base.contains("min(min_exec_time) AS min_ms"));
        // Pooled spread: each entry's own and how far its mean sits from the statement's.
        assert!(base.contains("sum(calls * (stddev_exec_time ^ 2 + mean_exec_time ^ 2))"));
        assert!(base.contains(" AND toplevel") && base.contains(&format!("NOT LIKE '%{TAG}'")));
        assert_eq!(base_relation(&base), Some("public.pg_stat_statements"));
        // The login's own text of it, compared by role name (never a role name parsed as an identifier, which an Entra
        // login like someone@example.com isn't), and without a subquery whose FROM would hide the relation.
        assert!(base.contains("FILTER (WHERE pg_get_userbyid(userid) = current_user))[1] AS own_query"), "{base}");
        assert!(!base.contains("regrole"), "{base}");
        let old = statements_base("public.pg_stat_statements", 120_000);
        assert!(
            old.contains("sum(total_time)") && !old.contains("toplevel") && old.contains("NULL::text AS first_seen")
        );
        assert!(statements_base("s.pg_stat_statements", 170_000).contains("min(stats_since)"));
        assert!(statements_base_legacy("s.pg_stat_statements").contains("GROUP BY queryid"));
    }

    #[test]
    fn a_family_counts_texts_not_statement_ids() {
        // The same text under one id per tenant schema is one variant; a different IN-list length is another.
        let sql = families_sql("SELECT 1");
        assert!(sql.contains("count(DISTINCT shape) AS variants"), "{sql}");
        assert!(sql.contains("HAVING count(DISTINCT shape) >= 5"));
    }

    #[test]
    fn rows_after_a_statistics_reset_come_from_the_planner() {
        // No vacuum or analyze since the counters started: n_live_tup counts only inserts minus deletes since.
        assert!(ROWS_SQL.contains("s.last_autoanalyze IS NULL THEN greatest(c.reltuples, s.n_live_tup)"));
        assert!(ROWS_SQL.contains("WHEN c.relkind = 'p' THEN NULL"));
        let parent = partition_parent("c");
        assert!(parent.contains("c.relispartition") && parent.contains("ih.inhrelid = c.oid"), "{parent}");
    }

    #[test]
    fn operator_classes_and_collations_per_key() {
        assert_eq!(opclasses(None), None);
        assert_eq!(opclasses(Some("")), None);
        assert_eq!(opclasses(Some("\u{1f}")), None, "every key on the defaults");
        assert_eq!(
            opclasses(Some("text_pattern_ops\u{1f}")),
            Some(vec!["text_pattern_ops".to_string(), String::new()])
        );
        assert_eq!(opclasses(Some("COLLATE \"C\"")), Some(vec!["COLLATE \"C\"".to_string()]));
    }

    #[test]
    fn a_generic_plan_says_what_it_cannot_know() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 5, "Total Cost": 4500.0, "Filter": "(customer_id = $1)"}}]"#;
        let mut plan = plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        mark_generic(&mut plan);
        assert!(plan.props["Generic Plan"].contains("default selectivities"));
        assert!(!plan.props.contains_key("search_path"));
    }

    #[test]
    fn identifiers_quote_only_when_needed() {
        assert_eq!(quote_ident("app_user"), "app_user");
        assert_eq!(quote_ident("AppUser"), "\"AppUser\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn cloud_detection() {
        assert_eq!(detect_cloud("rds", false, "x").as_deref(), Some("Amazon RDS"));
        assert_eq!(detect_cloud("", true, "x").as_deref(), Some("Amazon Aurora"));
        assert_eq!(
            detect_cloud("", false, "a.postgres.database.azure.com").as_deref(),
            Some("Azure Database for PostgreSQL")
        );
        assert_eq!(detect_cloud("", false, "localhost"), None);
    }
}
