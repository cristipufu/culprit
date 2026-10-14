use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use culprit_core::{
    Capability, CollectorNote, ColumnInfo, ConnectionSpec, Engine, Error, ForeignKey, IndexStat, MissingCapability,
    Plan, PlanKind, QueryStat, Result, SNAPSHOT_FORMAT, ServerInfo, Setting, Snapshot, SnapshotOptions, Spread,
    TableStat, WaitStat, Workload,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Value};

use crate::connect::{Connector, Flavor, Scope, TAG};
use crate::fingerprint::{fingerprint, matches_digest};
use crate::rows::{Rec, explain_one, query, query_one, query_with, tagged};
use crate::{plan_json, slowlog, vars};

const PS_GRANT: &str = "GRANT SELECT ON performance_schema.* TO";

/// What the probe learned about the server, shared by every collector.
pub struct Probe {
    pub version: String,
    pub comment: String,
    pub database: String,
    pub size_bytes: Option<f64>,
    pub started_at: Option<String>,
    pub now: String,
    pub user: String,
    pub cloud: Option<String>,
    pub performance_schema: bool,
    pub flavor: Flavor,
    /// Where the slow query log goes (`FILE`, `TABLE`, `FILE,TABLE`, `NONE`), whether it is on, and its threshold.
    pub log_output: String,
    pub slow_query_log: bool,
    pub long_query_time: Option<f64>,
    /// MariaDB's (and Percona's) per-table and per-index read counters; `None` where the server has no such setting.
    pub userstat: Option<bool>,
    /// `read_only` or `innodb_read_only` is on: how replicas (and Aurora's readers) are usually set up.
    pub read_only: bool,
}

impl Probe {
    pub fn product(&self) -> String {
        let short = self.version.split('-').next().unwrap_or(&self.version);
        format!("{} {short}", self.flavor.name())
    }

    fn grant_target(&self) -> String {
        match self.user.split_once('@') {
            Some((user, host)) => format!("'{}'@'{}'", user.replace('\'', "''"), host.replace('\'', "''")),
            None => format!("'{}'", self.user.replace('\'', "''")),
        }
    }

    /// The slow query log is written to `mysql.slow_log`, where Culprit can read it.
    pub fn slow_log_table(&self) -> bool {
        self.log_output.to_ascii_uppercase().split(',').any(|o| o.trim() == "TABLE")
    }

    /// The one statement that starts recording statements without a restart: the slow query log, into the table, at
    /// 100 ms (and, on MariaDB, per-index counters). `None` when everything it would set is already set.
    fn unlock_statement(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.userstat == Some(false) {
            parts.push("GLOBAL userstat = ON".into());
        }
        if !self.slow_query_log {
            parts.push("GLOBAL slow_query_log = ON".into());
        }
        if !self.slow_log_table() {
            let current = self.log_output.trim().to_ascii_uppercase();
            let output =
                if current.is_empty() || current == "NONE" { "TABLE".to_string() } else { format!("{current},TABLE") };
            parts.push(format!("GLOBAL log_output = '{output}'"));
        }
        if self.long_query_time.is_none_or(|t| t > 1.0) {
            parts.push("GLOBAL long_query_time = 0.1".into());
        }
        (!parts.is_empty()).then(|| format!("SET {};", parts.join(", ")))
    }

    fn performance_schema_restart(&self) -> String {
        format!(
            "For statistics on every statement (not only slow ones) and waits, set performance_schema = ON in the server configuration and restart {}.",
            self.flavor.name()
        )
    }

    /// How to get statement statistics when performance_schema is off: the no-restart path first.
    fn statements_fix(&self) -> String {
        match self.unlock_statement() {
            Some(sql) => format!(
                "Run {sql} {} then logs statements slower than long_query_time to mysql.slow_log, which Culprit reads: no restart needed. \
                 Connections opened earlier keep the old threshold until they reconnect, and the settings last until the next restart unless \
                 they are also added to the server configuration. {}",
                self.flavor.name(),
                self.performance_schema_restart()
            ),
            None => self.performance_schema_restart(),
        }
    }

    pub fn server_info(&self) -> ServerInfo {
        let mut capabilities = vec![Capability::TableStats, Capability::Sessions, Capability::Showplan];
        let mut missing = Vec::new();
        if self.performance_schema {
            capabilities.extend([Capability::PerformanceSchema, Capability::IndexUsage, Capability::QueryPlans]);
        } else {
            let reason = match self.flavor {
                Flavor::Mariadb => {
                    "performance_schema is off (MariaDB's default), so the server keeps no statement statistics."
                }
                Flavor::Mysql => "performance_schema is off, so MySQL keeps no statement statistics.",
            };
            missing.push(MissingCapability {
                capability: Capability::PerformanceSchema,
                reason: reason.into(),
                fix: Some(self.statements_fix()),
            });
            if self.userstat == Some(true) {
                capabilities.push(Capability::IndexUsage);
            } else {
                let (reason, fix) = match self.flavor {
                    Flavor::Mariadb => (
                        "MariaDB counts index reads only with performance_schema or userstat on.".to_string(),
                        "Run SET GLOBAL userstat = ON; to start counting (no restart needed), then scan again once the application has run for a while.".to_string(),
                    ),
                    Flavor::Mysql => (
                        "performance_schema is off, so MySQL counts no index use.".to_string(),
                        "Set performance_schema = ON in the server configuration and restart MySQL.".to_string(),
                    ),
                };
                missing.push(MissingCapability { capability: Capability::IndexUsage, reason, fix: Some(fix) });
            }
            if self.slow_query_log && self.slow_log_table() {
                capabilities.push(Capability::QueryPlans);
            } else {
                missing.push(MissingCapability {
                    capability: Capability::QueryPlans,
                    reason: "Plans come from the statements the server recorded, and it records none.".into(),
                    fix: None,
                });
            }
        }
        // Logged statements are a workload of their own without performance_schema, and samples to plan with it.
        if self.slow_query_log && self.slow_log_table() {
            capabilities.push(Capability::SlowLog);
        }
        ServerInfo {
            engine: Engine::Mysql,
            product: self.product(),
            version: self.version.clone(),
            edition: Some(self.comment.clone()).filter(|c| !c.is_empty()),
            cloud: self.cloud.clone(),
            database: self.database.clone(),
            database_size_bytes: self.size_bytes,
            started_at: self.started_at.clone(),
            read_only_session: true,
            capabilities,
            missing,
        }
    }
}

fn database_of(connector: &Connector) -> Result<String> {
    connector.database.clone().ok_or_else(|| {
        Error::invalid("A MySQL or MariaDB connection needs a database (schema) to analyze")
            .with_hint("Add it to the connection string, e.g. mysql://user@host:3306/shop")
    })
}

pub async fn probe(conn: &mut Conn, connector: &Connector) -> Result<Probe> {
    let database = database_of(connector)?;
    let row = query_one(
        conn,
        "SELECT VERSION() AS version, @@version_comment AS comment, @@basedir AS basedir, CURRENT_USER() AS user_name,
                @@performance_schema AS ps, DATE_FORMAT(UTC_TIMESTAMP(), '%Y-%m-%dT%H:%i:%sZ') AS now,
                @@log_output AS log_output, @@slow_query_log AS slow_log, @@long_query_time AS long_query_time,
                @@read_only AS read_only",
    )
    .await?
    .ok_or_else(|| Error::internal("The server returned no information about itself"))?;
    let size = query_with(
        conn,
        "SELECT SUM(DATA_LENGTH + INDEX_LENGTH) AS size, COUNT(*) AS tables FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
        vec![Value::from(database.clone())],
    )
    .await?
    .into_iter()
    .next();
    if size.as_ref().and_then(|r| r.number("tables")).unwrap_or(0.0) == 0.0 {
        let exists = query_with(
            conn,
            "SELECT 1 AS found FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            vec![Value::from(database.clone())],
        )
        .await?;
        if exists.is_empty() {
            return Err(Error::not_found(format!(
                "Database `{database}` does not exist or is not visible to this login"
            )));
        }
    }
    let uptime = vars::status(conn, &["Uptime"]).await.ok().and_then(|s| s.get("Uptime").copied());
    let started_at = match uptime {
        Some(seconds) => query_one(
            conn,
            &format!(
                "SELECT DATE_FORMAT(UTC_TIMESTAMP() - INTERVAL {} SECOND, '%Y-%m-%dT%H:%i:%sZ') AS started",
                seconds as u64
            ),
        )
        .await?
        .map(|r| r.string("started")),
        None => None,
    };
    let version = row.string("version");
    let comment = row.string("comment");
    let flavor = Flavor::of_version(&version);
    let cloud = detect_cloud(conn, &connector.host, &row.string("basedir"), &comment, flavor).await;
    let userstat = match flavor {
        Flavor::Mariadb => {
            query_one(conn, "SELECT @@userstat AS userstat").await.ok().flatten().map(|r| r.flag("userstat"))
        }
        Flavor::Mysql => None,
    };
    Ok(Probe {
        flavor,
        log_output: row.string("log_output"),
        slow_query_log: row.flag("slow_log"),
        long_query_time: row.number("long_query_time"),
        userstat,
        version,
        comment,
        database,
        size_bytes: size.and_then(|r| r.number("size")),
        started_at,
        now: row.string("now"),
        user: row.string("user_name"),
        cloud,
        performance_schema: row.flag("ps"),
        // innodb_read_only (Aurora's readers) is read on its own: a server without it must not fail the probe.
        read_only: row.flag("read_only")
            || query_one(conn, "SELECT @@innodb_read_only AS v").await.ok().flatten().is_some_and(|r| r.flag("v")),
    })
}

async fn detect_cloud(conn: &mut Conn, host: &str, basedir: &str, comment: &str, flavor: Flavor) -> Option<String> {
    let host = host.to_ascii_lowercase();
    if host.ends_with(".mysql.database.azure.com") {
        return Some("Azure Database for MySQL".into());
    }
    if host.ends_with(".mariadb.database.azure.com") {
        return Some("Azure Database for MariaDB".into());
    }
    if host.contains(".skysql.") || host.ends_with(".mariadb.com") {
        return Some("MariaDB Cloud".into());
    }
    let aurora =
        flavor == Flavor::Mysql && query_one(conn, "SELECT AURORA_VERSION() AS v").await.ok().flatten().is_some();
    if aurora {
        return Some("Amazon Aurora".into());
    }
    if host.contains(".rds.amazonaws.com") || basedir.contains("/rdsdbbin/") {
        return Some("Amazon RDS".into());
    }
    if comment.to_ascii_lowercase().contains("google") || basedir.contains("/cloudsql") {
        return Some("Google Cloud SQL".into());
    }
    None
}

/// Collect a full snapshot. Each collector that fails becomes a note; only an unreachable server is an error.
pub async fn snapshot(
    connector: &Connector,
    spec: &ConnectionSpec,
    connection: &str,
    options: &SnapshotOptions,
) -> Result<Snapshot> {
    let started = Instant::now();
    let mut primary = connector.connect(Scope::Server).await?;
    let probe = probe(&mut primary, connector).await?;
    let db = probe.database.clone();

    let (workload, schema, misc) = tokio::join!(
        async {
            let mut conn = primary;
            collect_workload(&mut conn, connector, &probe, options).await
        },
        async {
            let mut conn = connector.connect(Scope::Server).await?;
            let tables = tables(&mut conn, &db).await;
            let columns = columns(&mut conn, &db).await;
            let indexes = indexes(&mut conn, &db, &probe).await;
            let fks = foreign_keys(&mut conn, &db).await;
            Ok::<_, Error>((tables, columns, indexes, fks))
        },
        async {
            let mut conn = connector.connect(Scope::Server).await?;
            let mut settings = settings(&mut conn, probe.flavor).await;
            if let (Ok(settings), Some(role)) = (settings.as_mut(), replica_role(&mut conn, &probe).await) {
                settings.push(Setting {
                    name: culprit_core::REPLICA_SETTING.into(),
                    value: role.into(),
                    default_value: None,
                    scope: "server".into(),
                });
            }
            let waits = crate::pressure::waits(&mut conn).await;
            let pressure = crate::pressure::collect(&mut conn).await;
            Ok::<_, Error>((settings, waits, pressure))
        },
    );

    let mut server = probe.server_info();
    let mut notes = Vec::new();
    let mut note =
        |collector: &str, message: String| notes.push(CollectorNote { collector: collector.into(), message });

    let mut source_waits = Vec::new();
    let workload = match workload {
        Ok(Some(collected)) => {
            source_waits = collected.waits;
            for (collector, message) in collected.notes {
                note(collector, message);
            }
            if collected.workload.source == slowlog::SOURCE && !server.has(Capability::SlowLog) {
                server.capabilities.push(Capability::SlowLog);
            }
            let planned = collected.workload.queries.iter().any(|q| q.plan.is_some());
            if planned {
                server.missing.retain(|m| m.capability != Capability::QueryPlans);
                if !server.has(Capability::QueryPlans) {
                    server.capabilities.push(Capability::QueryPlans);
                }
            } else if let Some((reason, fix)) = collected.no_plans {
                server.missing.retain(|m| m.capability != Capability::QueryPlans);
                mark_missing(&mut server, Capability::QueryPlans, reason, fix);
            } else {
                server.capabilities.retain(|c| *c != Capability::QueryPlans);
            }
            Some(collected.workload)
        }
        Ok(None) => None,
        Err(e) => {
            note("workload", e.message.clone());
            mark_missing(
                &mut server,
                Capability::PerformanceSchema,
                format!("Reading statement digests failed: {}", e.message),
                Some(format!("{PS_GRANT} {};", probe.grant_target())),
            );
            mark_missing(&mut server, Capability::QueryPlans, "Plans come from the statement digests.".into(), None);
            None
        }
    };

    let (mut tables_out, mut columns_out, mut indexes_out, mut fks_out) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    match schema {
        Ok((tables, columns, indexes, fks)) => {
            match tables {
                Ok(t) => tables_out = t,
                Err(e) => {
                    note("tables", e.message.clone());
                    mark_missing(
                        &mut server,
                        Capability::TableStats,
                        format!("Reading tables failed: {}", e.message),
                        e.hint,
                    );
                }
            }
            match columns {
                Ok(c) => columns_out = c,
                Err(e) => note("columns", e.message),
            }
            match indexes {
                Ok((i, usage_note)) => {
                    indexes_out = i;
                    if let Some(message) = usage_note {
                        note("index_usage", message.clone());
                        mark_missing(
                            &mut server,
                            Capability::IndexUsage,
                            message,
                            Some(format!("{PS_GRANT} {};", probe.grant_target())),
                        );
                    }
                }
                Err(e) => {
                    note("indexes", e.message.clone());
                    mark_missing(
                        &mut server,
                        Capability::IndexUsage,
                        format!("Reading indexes failed: {}", e.message),
                        e.hint,
                    );
                }
            }
            match fks {
                Ok(f) => fks_out = f,
                Err(e) => note("foreign_keys", e.message),
            }
        }
        Err(e) => {
            note("schema", e.message.clone());
            mark_missing(&mut server, Capability::TableStats, e.message.clone(), e.hint.clone());
            mark_missing(&mut server, Capability::IndexUsage, e.message, e.hint);
        }
    }

    let (mut settings_out, mut waits_out, mut pressure_out) = (Vec::new(), Vec::new(), None);
    match misc {
        Ok((settings, waits, (pressure, pressure_notes))) => {
            for message in pressure_notes {
                note("pressure", message);
            }
            pressure_out = Some(pressure);
            match settings {
                Ok(s) => settings_out = s,
                Err(e) => note("settings", e.message),
            }
            match waits {
                Ok(w) if !w.is_empty() => {
                    waits_out = w;
                    server.capabilities.push(Capability::WaitStats);
                }
                Ok(_) => {}
                Err(e) => note("waits", e.message),
            }
            if waits_out.is_empty() {
                waits_out = std::mem::take(&mut source_waits);
            }
        }
        Err(e) => note("settings", e.message),
    }

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
        statistics: Vec::new(),
        waits: waits_out,
        resources: Vec::new(),
        settings: settings_out,
        pressure: pressure_out,
        notes,
        options: Some(options.clone()),
        query_families: Vec::new(),
        plan_cache: None,
        tuning: None,
        deadlocks: None,
        constraints: Vec::new(),
        histograms: None,
        escalations_before: None,
        sizes_before: None,
    };
    crate::signals::collect(connector, &probe, &mut snapshot).await;
    snapshot.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(snapshot)
}

fn mark_missing(server: &mut ServerInfo, capability: Capability, reason: String, fix: Option<String>) {
    server.capabilities.retain(|c| *c != capability);
    if server.missing.iter().any(|m| m.capability == capability) {
        return;
    }
    server.missing.push(MissingCapability { capability, reason, fix });
}

struct CollectedWorkload {
    workload: Workload,
    /// Waits the workload source measured itself (the slow query log's lock time), for servers without wait totals.
    waits: Vec<WaitStat>,
    notes: Vec<(&'static str, String)>,
    /// Why no statement could be planned and what would change that, when none was.
    no_plans: Option<(String, Option<String>)>,
}

struct Digest {
    stat: QueryStat,
    sample: Option<String>,
    /// Where `sample` came from, said on the plan made from it.
    sample_from: SampleFrom,
    rows_examined: f64,
}

/// Where the statement a digest is planned from came from: its values are that run's, which may not be typical.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SampleFrom {
    /// The digest table's QUERY_SAMPLE_TEXT: the slowest run of the last minute or so (performance_schema keeps the
    /// sample with the longest wait until it ages out), so its values lean to an outlier's.
    Digest,
    /// The statement history: the latest run.
    History,
    /// The slow query log: a run slower than long_query_time.
    SlowLog,
}

impl SampleFrom {
    fn sentence(self) -> &'static str {
        match self {
            SampleFrom::Digest => {
                "Planned from the sample performance_schema keeps for this statement: its slowest recent run, so the values may be an outlier's rather than a typical run's."
            }
            SampleFrom::History => "Planned from the latest run in performance_schema's statement history.",
            SampleFrom::SlowLog => {
                "Planned from a run in the slow query log: a slow run, so the values may be an outlier's rather than a typical run's."
            }
        }
    }
}

const PICOS_PER_MS: f64 = 1e9;

async fn digest_columns(conn: &mut Conn) -> Result<Vec<String>> {
    Ok(query(
        conn,
        "SELECT COLUMN_NAME AS name FROM information_schema.COLUMNS
          WHERE TABLE_SCHEMA = 'performance_schema' AND TABLE_NAME = 'events_statements_summary_by_digest'",
    )
    .await?
    .into_iter()
    .map(|r| r.string("name").to_ascii_uppercase())
    .collect())
}

async fn collect_workload(
    conn: &mut Conn,
    connector: &Connector,
    probe: &Probe,
    options: &SnapshotOptions,
) -> Result<Option<CollectedWorkload>> {
    if !probe.performance_schema {
        return match probe.slow_log_table() {
            true => slow_log_workload(conn, connector, probe, options).await,
            false => Ok(None),
        };
    }
    let available = digest_columns(conn).await?;
    let has = |c: &str| available.iter().any(|a| a == c);
    let optional = |c: &str| if has(c) { c.to_string() } else { format!("NULL AS {c}") };
    // Rows examined stands in for "logical reads": MySQL counts rows the storage engine handed to the server, not pages.
    // EXPLAINs and ANALYZEs are Culprit's own planning; `SELECT @@…` digests are driver handshakes, never actionable.
    let sql = format!(
        "SELECT DIGEST AS digest, DIGEST_TEXT AS digest_text, COUNT_STAR AS calls, SUM_TIMER_WAIT AS total_ps,
                MAX_TIMER_WAIT AS max_ps, MIN_TIMER_WAIT AS min_ps, SUM_ROWS_SENT AS rows_sent, SUM_ROWS_EXAMINED AS rows_examined,
                SUM_CREATED_TMP_DISK_TABLES AS tmp_disk, SUM_NO_INDEX_USED AS no_index, SUM_LOCK_TIME AS lock_ps,
                {cpu}, {sample}, {p95}, {p99}, {affected},
                DATE_FORMAT(FIRST_SEEN, '%Y-%m-%dT%H:%i:%sZ') AS first_seen,
                DATE_FORMAT(LAST_SEEN, '%Y-%m-%dT%H:%i:%sZ') AS last_seen
           FROM performance_schema.events_statements_summary_by_digest
          WHERE SCHEMA_NAME = ? AND DIGEST IS NOT NULL AND DIGEST_TEXT NOT LIKE 'EXPLAIN %'
            AND DIGEST_TEXT NOT LIKE 'ANALYZE FORMAT%' AND DIGEST_TEXT NOT LIKE 'SHOW ANALYZE%'
            AND DIGEST_TEXT NOT LIKE 'SHOW EXPLAIN%' AND DIGEST_TEXT NOT LIKE 'SELECT @@%'",
        cpu = optional("SUM_CPU_TIME"),
        sample = optional("QUERY_SAMPLE_TEXT"),
        p95 = optional("QUANTILE_95"),
        p99 = optional("QUANTILE_99"),
        affected = optional("SUM_ROWS_AFFECTED"),
    );
    let rows = query_with(conn, &sql, vec![Value::from(probe.database.clone())]).await?;
    let all: Vec<Digest> = rows
        .iter()
        .filter(|r| !r.text("query_sample_text").is_some_and(|s| s.contains(TAG)))
        .map(|r| digest(r, has("SUM_CPU_TIME")))
        .collect();
    let total_time_ms: f64 = all.iter().map(|d| d.stat.total_time_ms).sum();
    // SUM_CPU_TIME stays 0 unless CPU instrumentation is on; all zeros means unknown, not free.
    let total_cpu_ms: Option<f64> = has("SUM_CPU_TIME")
        .then(|| all.iter().filter_map(|d| d.stat.cpu_time_ms).sum::<f64>())
        .filter(|cpu| *cpu > 0.0);
    let mut all = all;
    if total_cpu_ms.is_none() {
        all.iter_mut().for_each(|d| d.stat.cpu_time_ms = None);
    }
    let total_executions: f64 = all.iter().map(|d| d.stat.executions).sum();
    let query_count = all.len() as f64;
    let window_start = all.iter().filter_map(|d| d.stat.first_seen.clone()).min().or(probe.started_at.clone());
    let window_hours =
        window_start.as_deref().and_then(|start| hours_between(start, &probe.now)).unwrap_or(0.0).max(1.0 / 60.0);

    let mut kept = select_top(all, options.top_queries as usize);
    attach_histograms(conn, &probe.database, &mut kept).await;
    for d in kept.iter_mut() {
        let max = d.stat.max_time_ms;
        if let Some(spread) = d.stat.spread.as_mut() {
            spread.clamp_to(max);
        }
    }
    let mut notes = Vec::new();
    let mut no_plans = None;
    if !kept.is_empty() && options.plans > 0 {
        let limit = options.plans as usize;
        let planned_part = limit.min(kept.len());
        attach_samples(conn, probe, options, &mut kept[..planned_part]).await;
        let outcome = attach_plans(connector, &probe.database, &mut kept, limit).await;
        if let Some(message) = outcome.note() {
            notes.push(("plans", message));
        }
        if outcome.planned == 0 {
            no_plans = Some(no_plans_reason(conn, probe, has("QUERY_SAMPLE_TEXT")).await);
        }
    }
    let queries = kept.into_iter().map(|d| d.stat).collect();
    let untracked_time_ms = untracked_ms(conn).await;
    if let Some(ms) = untracked_time_ms {
        notes.push((
            "workload",
            format!(
                "performance_schema's digest table is full (performance_schema_digests_size), so {} of statement time across the server went into one catch-all row and isn't in these totals: shares are of the time it could attribute.",
                culprit_core::format::duration(ms)
            ),
        ));
    }
    Ok(Some(CollectedWorkload {
        waits: Vec::new(),
        workload: Workload {
            source: "performance_schema".into(),
            windowed: false,
            window_start,
            window_end: Some(probe.now.clone()),
            window_hours,
            total_time_ms,
            total_cpu_ms,
            total_executions,
            query_count,
            queries,
            timeline: None,
            untracked_time_ms,
            min_logged_ms: None,
        },
        notes,
        no_plans,
    }))
}

/// Statement time performance_schema couldn't keep per digest once its table was full: the row with no digest. It is
/// the whole server's (it has no schema). `None` when there is none, or it can't be read.
async fn untracked_ms(conn: &mut Conn) -> Option<f64> {
    query_one(
        conn,
        "SELECT SUM_TIMER_WAIT AS total_ps FROM performance_schema.events_statements_summary_by_digest WHERE DIGEST IS NULL",
    )
    .await
    .ok()
    .flatten()
    .and_then(|r| r.number("total_ps"))
    .map(|ps| ps / PICOS_PER_MS)
    .filter(|ms| *ms > 0.0)
}

/// Statements recorded in the slow query log table over the scan window, grouped by shape.
async fn slow_log_workload(
    conn: &mut Conn,
    connector: &Connector,
    probe: &Probe,
    options: &SnapshotOptions,
) -> Result<Option<CollectedWorkload>> {
    let now = culprit_core::changes::epoch(&probe.now).unwrap_or(0.0);
    let since = slowlog::iso(now - f64::from(options.window_in_minutes()) * 60.0);
    let (runs, truncated) = slowlog::read_runs(conn, &probe.database, &slowlog::sql_time(&since), None).await?;
    if runs.is_empty() {
        return Ok(None);
    }
    let collected =
        slowlog::workload(&runs, &since, &probe.now, probe.long_query_time, options.top_queries as usize, truncated);
    let samples: HashMap<String, String> = collected.samples.into_iter().collect();
    let mut kept: Vec<Digest> = collected
        .workload
        .queries
        .iter()
        .map(|q| Digest {
            sample: samples.get(&q.id).cloned(),
            sample_from: SampleFrom::SlowLog,
            rows_examined: 0.0,
            stat: q.clone(),
        })
        .collect();
    let mut notes = vec![("workload", collected.threshold_note)];
    let mut no_plans = None;
    if options.plans > 0 {
        let outcome = attach_plans(connector, &probe.database, &mut kept, options.plans as usize).await;
        if let Some(message) = outcome.note() {
            notes.push(("plans", message));
        }
        if outcome.planned == 0 {
            no_plans = Some(("No logged statement could be planned.".to_string(), None));
        }
    }
    let mut workload = collected.workload;
    workload.queries = kept.into_iter().map(|d| d.stat).collect();
    let waits = (collected.lock_ms > 0.0)
        .then(|| WaitStat {
            category: "Lock".into(),
            wait_type: Some("lock time (slow query log)".into()),
            wait_ms: collected.lock_ms,
            tasks: None,
        })
        .into_iter()
        .collect();
    Ok(Some(CollectedWorkload { workload, waits, notes, no_plans }))
}

fn digest(r: &Rec, has_cpu: bool) -> Digest {
    let ms = |key: &str| r.number(key).map(|p| p / PICOS_PER_MS);
    let executions = r.number("calls").unwrap_or(0.0);
    let total = r.number("total_ps").unwrap_or(0.0) / PICOS_PER_MS;
    let digest = r.string("digest");
    Digest {
        stat: QueryStat {
            id: digest.chars().take(16).collect(),
            query_hash: Some(digest),
            text: r.string("digest_text"),
            parameters: None,
            object_name: None,
            executions,
            total_time_ms: total,
            mean_time_ms: if executions > 0.0 { total / executions } else { 0.0 },
            max_time_ms: r.number("max_ps").map(|p| p / PICOS_PER_MS),
            stddev_time_ms: None,
            cpu_time_ms: if has_cpu { r.number("sum_cpu_time").map(|p| p / PICOS_PER_MS) } else { None },
            rows: r.number("rows_sent"),
            logical_reads: r.number("rows_examined"),
            physical_reads: None,
            temp_writes: r.number("tmp_disk"),
            first_seen: r.text("first_seen"),
            last_seen: r.text("last_seen"),
            plans: Vec::new(),
            plan: None,
            // Lock time (table, metadata and, since 8.0.28, InnoDB row locks) is the wait MySQL attributes to statements.
            waits: r
                .number("lock_ps")
                .map(|p| p / PICOS_PER_MS)
                .filter(|ms| *ms > 0.0)
                .map(|ms| {
                    vec![WaitStat {
                        category: "Lock".into(),
                        wait_type: Some("lock time".into()),
                        wait_ms: ms,
                        tasks: None,
                    }]
                })
                .unwrap_or_default(),
            trend: None,
            // Percentiles exist from 8.0.31; the histogram, attached later, from 8.0.19.
            spread: Some(Spread {
                min_ms: ms("min_ps"),
                p95_ms: ms("quantile_95"),
                p99_ms: ms("quantile_99"),
                lock_ms: ms("lock_ps"),
                ..Default::default()
            }),
            costs: None,
            // `rows` is what the statement sent back (none for an UPDATE); this is what it changed.
            rows_affected: r.number("sum_rows_affected"),
            plan_id: None,
        },
        sample: r.text("query_sample_text"),
        sample_from: SampleFrom::Digest,
        rows_examined: r.number("rows_examined").unwrap_or(0.0),
    }
}

/// MySQL 8.0.19+ keeps a run-time histogram per digest: enough to tell a long tail from a uniformly slow statement.
/// Older servers have no histogram table; the spread then shows fastest, average and slowest only.
async fn attach_histograms(conn: &mut Conn, database: &str, kept: &mut [Digest]) {
    let digests: Vec<String> = kept.iter().filter_map(|d| d.stat.query_hash.clone()).collect();
    if digests.is_empty() {
        return;
    }
    let marks = vec!["?"; digests.len()].join(", ");
    let sql = format!(
        "SELECT DIGEST AS digest, BUCKET_TIMER_LOW AS low_ps, BUCKET_TIMER_HIGH AS high_ps, COUNT_BUCKET AS runs
           FROM performance_schema.events_statements_histogram_by_digest
          WHERE SCHEMA_NAME = ? AND DIGEST IN ({marks}) AND COUNT_BUCKET > 0
          ORDER BY DIGEST, BUCKET_NUMBER"
    );
    let mut params = vec![Value::from(database.to_string())];
    params.extend(digests.into_iter().map(Value::from));
    let Ok(rows) = query_with(conn, &sql, params).await else { return };
    let mut by_digest: HashMap<String, Vec<[f64; 3]>> = HashMap::new();
    for r in rows {
        let low = r.number("low_ps").unwrap_or(0.0) / PICOS_PER_MS;
        let high = r.number("high_ps").unwrap_or(0.0) / PICOS_PER_MS;
        by_digest.entry(r.string("digest")).or_default().push([low, high, r.number("runs").unwrap_or(0.0)]);
    }
    for d in kept.iter_mut() {
        let Some(buckets) = d.stat.query_hash.as_ref().and_then(|h| by_digest.remove(h)) else { continue };
        let spread = d.stat.spread.get_or_insert_with(Spread::default);
        spread.p50_ms = quantile(&buckets, 0.5);
        spread.p95_ms = spread.p95_ms.or(quantile(&buckets, 0.95));
        spread.p99_ms = spread.p99_ms.or(quantile(&buckets, 0.99));
        spread.histogram = fold_buckets(&buckets, 32);
    }
}

/// The upper edge of the bucket where the running count reaches `q` of all runs.
fn quantile(buckets: &[[f64; 3]], q: f64) -> Option<f64> {
    let total: f64 = buckets.iter().map(|b| b[2]).sum();
    if total <= 0.0 {
        return None;
    }
    let mut seen = 0.0;
    for b in buckets {
        seen += b[2];
        if seen >= total * q {
            return Some(b[1]);
        }
    }
    buckets.last().map(|b| b[1])
}

/// MySQL's histogram has 450 narrow buckets; neighbours merge until at most `max` remain.
fn fold_buckets(buckets: &[[f64; 3]], max: usize) -> Vec<[f64; 3]> {
    if buckets.len() <= max {
        return buckets.to_vec();
    }
    let per = buckets.len().div_ceil(max);
    buckets.chunks(per).map(|c| [c[0][0], c[c.len() - 1][1], c.iter().map(|b| b[2]).sum()]).collect()
}

/// Keep the heaviest digests by time, plus the heaviest by executions and rows examined, so chatty and scan-heavy
/// statements are not crowded out. The result is ordered by time, heaviest first.
fn select_top(all: Vec<Digest>, limit: usize) -> Vec<Digest> {
    if limit == 0 || all.is_empty() {
        return Vec::new();
    }
    let ranked = |key: &dyn Fn(&Digest) -> f64| -> Vec<usize> {
        let mut idx: Vec<usize> = (0..all.len()).collect();
        idx.sort_by(|&a, &b| key(&all[b]).total_cmp(&key(&all[a])));
        idx
    };
    let by_time = ranked(&|d| d.stat.total_time_ms);
    let by_calls = ranked(&|d| d.stat.executions);
    let by_examined = ranked(&|d| d.rows_examined);
    let other = (limit / 4).max(1);
    let mut chosen: Vec<usize> = Vec::new();
    let candidates = by_time
        .iter()
        .take(limit.saturating_sub(2 * other).max(1))
        .chain(by_calls.iter().take(other))
        .chain(by_examined.iter().take(other))
        .chain(by_time.iter());
    for &i in candidates {
        if chosen.len() >= limit {
            break;
        }
        if !chosen.contains(&i) {
            chosen.push(i);
        }
    }
    let mut kept: Vec<Digest> =
        all.into_iter().enumerate().filter(|(i, _)| chosen.contains(i)).map(|(_, d)| d).collect();
    kept.sort_by(|a, b| b.stat.total_time_ms.total_cmp(&a.stat.total_time_ms));
    kept
}

fn plannable(sample: &str) -> bool {
    let trimmed = sample.trim_start().trim_start_matches('(').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let starts = ["select", "with", "table "].iter().any(|k| lower.starts_with(k));
    // A sample cut at performance_schema_max_sql_text_length, or a server-side prepared statement, can't be planned.
    starts && !trimmed.ends_with("...") && !trimmed.contains(" ?") && !trimmed.contains("=?")
}

#[derive(Default)]
struct PlanOutcome {
    planned: usize,
    /// Reads the server kept no text for.
    no_text: usize,
    /// Reads whose text was cut short or prepared with placeholders.
    unusable: usize,
    /// Statements that change data (or aren't queries): Culprit plans reads only.
    not_reads: usize,
    /// Samples whose values matched nothing, so MySQL showed no operators ("no matching row in const table").
    empty: usize,
    failed: usize,
    error: Option<String>,
}

impl PlanOutcome {
    fn note(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("Could not open a session to plan statements: {error}"));
        }
        if self.no_text + self.unusable + self.failed + self.empty == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if self.no_text > 0 {
            parts.push(format!("{} had no recorded statement text", self.no_text));
        }
        if self.unusable > 0 {
            parts.push(format!("{} had only truncated or server-side prepared text", self.unusable));
        }
        if self.empty > 0 {
            parts.push(format!(
                "{} {} for values that matched no rows, so the server showed no plan",
                self.empty,
                if self.empty == 1 { "was sampled" } else { "were sampled" }
            ));
        }
        if self.failed > 0 {
            parts.push(format!("{} could not be explained", self.failed));
        }
        let queries = if self.planned == 1 { "query" } else { "queries" };
        Some(format!("Planned {} {queries}; {}.", self.planned, parts.join(", ")))
    }
}

/// Whether a statement is a query Culprit would plan (by its digest text or a sample).
fn reads_shape(text: &str) -> bool {
    let lower = text.trim_start().trim_start_matches('(').trim_start().to_ascii_lowercase();
    ["select", "with", "table "].iter().any(|k| lower.starts_with(k))
}

async fn attach_plans(connector: &Connector, database: &str, queries: &mut [Digest], limit: usize) -> PlanOutcome {
    let mut outcome = PlanOutcome::default();
    let mut conn = match connector.connect(Scope::Database).await {
        Ok(conn) => conn,
        Err(e) => {
            outcome.error = Some(e.message);
            return outcome;
        }
    };
    if connector.flavor() == Flavor::Mysql {
        let _ = conn.query_drop(tagged("SET SESSION explain_json_format_version = 1")).await;
    }
    for digest in queries.iter_mut().take(limit) {
        // Sample text was written by whoever ran the statement: it must lex as exactly one statement, and EXPLAIN goes
        // through a prepared statement, which can't hold a second one.
        let sample = digest.sample.as_deref().filter(|s| plannable(s));
        let Some(sample) = sample.and_then(|s| culprit_core::sql::single_statement(s, Engine::Mysql).ok()) else {
            match (&digest.sample, reads_shape(&digest.stat.text)) {
                (_, false) => outcome.not_reads += 1,
                (None, true) => outcome.no_text += 1,
                (Some(_), true) => outcome.unusable += 1,
            }
            continue;
        };
        let sql = format!("EXPLAIN FORMAT=JSON {sample}");
        match explain_one(&mut conn, &sql).await {
            Ok(Some(row)) => {
                let json = row.text("explain").or_else(|| row.first_text()).unwrap_or_default();
                match sample_plan(&json, sample, database, &digest.stat.text) {
                    // Nothing under the query block: the sample's values matched no row and MySQL stopped there. An
                    // empty diagram would read as "this query does nothing"; say why instead.
                    Ok(plan) if plan.root.children.is_empty() => outcome.empty += 1,
                    Ok(mut plan) => {
                        if let Some(version) = connector.version() {
                            plan.props.insert("server_version".into(), version.to_string());
                        }
                        // A simple MariaDB plan reads like MySQL's JSON; say which server made it so notes name it.
                        if connector.flavor() == Flavor::Mariadb {
                            plan.props.insert("flavor".into(), "MariaDB".into());
                        }
                        plan.props.insert("Sample".into(), digest.sample_from.sentence().into());
                        digest.stat.plan = Some(plan);
                        outcome.planned += 1;
                    }
                    Err(_) => outcome.failed += 1,
                }
            }
            _ => outcome.failed += 1,
        }
    }
    let _ = conn.disconnect().await;
    outcome
}

/// The plan of a captured sample, kept without the sample's values. A sample is someone else's real statement (an
/// email, a token, an id in a WHERE clause) and MySQL copies its literals into the plan's conditions and JSON; the
/// report, the UI and coding agents get the digest's text and a plan whose literals are placeholders. LIKE patterns
/// keep their wildcard shape (`'%?'`), so a leading wildcard is still visible.
fn sample_plan(json: &str, sample: &str, database: &str, digest_text: &str) -> Result<Plan> {
    let plan = plan_json::parse(json, PlanKind::Estimated, Some(sample), Some(database))?;
    let mut clean = culprit_core::redact::redact_plan(&plan);
    clean.raw = plan.raw.as_deref().map(|raw| culprit_core::redact::redact_native(raw, Engine::Mysql));
    clean.statement = Some(digest_text.to_string());
    Ok(clean)
}

/// Statement text to plan each digest by. MySQL 8 keeps a sample per digest; MariaDB (and older MySQL) only keep the
/// statements in the history tables, and the slow query log table has real statements of the same shape.
async fn attach_samples(conn: &mut Conn, probe: &Probe, options: &SnapshotOptions, kept: &mut [Digest]) {
    let wanted = |d: &Digest| reads_shape(&d.stat.text) && !d.sample.as_deref().is_some_and(plannable);
    let digests: Vec<String> = kept.iter().filter(|d| wanted(d)).filter_map(|d| d.stat.query_hash.clone()).collect();
    if !digests.is_empty() {
        let marks = vec!["?"; digests.len()].join(", ");
        let mut found: HashMap<String, String> = HashMap::new();
        for table in ["events_statements_history_long", "events_statements_history", "events_statements_current"] {
            let sql = format!(
                "SELECT DIGEST AS digest, SQL_TEXT AS sql_text FROM performance_schema.{table}
                  WHERE CURRENT_SCHEMA = ? AND DIGEST IN ({marks}) AND SQL_TEXT IS NOT NULL
                  ORDER BY TIMER_START DESC"
            );
            let mut params = vec![Value::from(probe.database.clone())];
            params.extend(digests.iter().cloned().map(Value::from));
            for r in query_with(conn, &sql, params).await.unwrap_or_default() {
                if let (Some(digest), Some(text)) = (r.text("digest"), r.text("sql_text"))
                    && !text.contains(TAG)
                    && plannable(&text)
                {
                    found.entry(digest).or_insert(text);
                }
            }
        }
        for d in kept.iter_mut().filter(|d| wanted(d)) {
            if let Some(text) = d.stat.query_hash.as_ref().and_then(|h| found.remove(h)) {
                d.sample = Some(text);
                d.sample_from = SampleFrom::History;
            }
        }
    }
    if probe.slow_log_table() && kept.iter().any(&wanted) {
        let now = culprit_core::changes::epoch(&probe.now).unwrap_or(0.0);
        let since = slowlog::iso(now - f64::from(options.window_in_minutes()) * 60.0);
        if let Ok(runs) = slowlog::read(conn, &probe.database, &slowlog::sql_time(&since), None).await {
            let keyed: Vec<(String, &str)> = runs.iter().map(|r| (fingerprint(&r.sql).key, r.sql.as_str())).collect();
            for d in kept.iter_mut().filter(|d| wanted(d)) {
                if let Some((_, text)) =
                    keyed.iter().find(|(key, text)| plannable(text) && matches_digest(key, &d.stat.text))
                {
                    d.sample = Some(text.to_string());
                    d.sample_from = SampleFrom::SlowLog;
                }
            }
        }
    }
}

/// Why nothing could be planned, and the change that would let Culprit plan statements. `sample_text`: the digest
/// table keeps a sample statement per digest (MySQL 8.0.3+); without it the statement history is the only text.
async fn no_plans_reason(conn: &mut Conn, probe: &Probe, sample_text: bool) -> (String, Option<String>) {
    // The history tables fill only under the current-statement consumer.
    let off: Vec<String> = query(
        conn,
        "SELECT NAME AS name FROM performance_schema.setup_consumers
          WHERE NAME IN ('events_statements_current', 'events_statements_history_long') AND ENABLED = 'NO'",
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| r.string("name"))
    .collect();
    if !sample_text && !off.is_empty() {
        let names = ["events_statements_current", "events_statements_history_long"];
        let config =
            names.iter().map(|n| format!("performance_schema_consumer_{n} = ON")).collect::<Vec<_>>().join(" and ");
        let why = match probe.flavor {
            Flavor::Mariadb => "MariaDB keeps statement digests but not the statements themselves",
            Flavor::Mysql => {
                "This MySQL keeps statement digests but no sample statement for each (MySQL 8.0 added one)"
            }
        };
        return (
            format!("{why}, so there is no text to plan."),
            Some(format!(
                "Run UPDATE performance_schema.setup_consumers SET ENABLED = 'YES' WHERE NAME IN ('events_statements_current', 'events_statements_history_long'); \
                 to keep the latest statements (no restart needed; add {config} to the server configuration to keep them after a restart). \
                 Or open a statement in the editor and Explain it there."
            )),
        );
    }
    (
        "None of the recorded statements could be planned: their text was truncated, prepared with placeholders, or no longer in the statement history.".into(),
        (!sample_text).then(|| {
            format!(
                "{}'s statement history keeps only the latest statements (performance_schema_events_statements_history_long_size). \
                 Logging slow statements to the mysql.slow_log table gives Culprit lasting samples: SET GLOBAL slow_query_log = ON, GLOBAL log_output = 'FILE,TABLE';",
                probe.flavor.name()
            )
        }),
    )
}

/// Hours between two RFC 3339 UTC timestamps.
fn hours_between(start: &str, end: &str) -> Option<f64> {
    Some((epoch(end)? - epoch(start)?) / 3600.0)
}

fn epoch(text: &str) -> Option<f64> {
    let (date, time) = text.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|p| p.parse::<f64>());
    let (h, mi, s) = (t.next()?.ok()?, t.next()?.ok()?, t.next().unwrap_or(Ok(0.0)).ok()?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days as f64 * 86_400.0 + h * 3600.0 + mi * 60.0 + s)
}

async fn tables(conn: &mut Conn, db: &str) -> Result<Vec<TableStat>> {
    let rows = query_with(
        conn,
        "SELECT t.TABLE_NAME AS name, t.TABLE_ROWS AS row_estimate, t.DATA_LENGTH AS data_bytes, t.INDEX_LENGTH AS index_bytes,
                EXISTS (SELECT 1 FROM information_schema.TABLE_CONSTRAINTS c
                         WHERE c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME
                           AND c.CONSTRAINT_TYPE = 'PRIMARY KEY') AS has_pk
           FROM information_schema.TABLES t
          WHERE t.TABLE_SCHEMA = ? AND t.TABLE_TYPE = 'BASE TABLE'",
        vec![Value::from(db)],
    )
    .await?;
    let io: HashMap<String, Rec> = query_with(
        conn,
        "SELECT OBJECT_NAME AS name, COUNT_INSERT AS inserts, COUNT_UPDATE AS updates, COUNT_DELETE AS deletes
           FROM performance_schema.table_io_waits_summary_by_table WHERE OBJECT_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| (r.string("name").to_ascii_lowercase(), r))
    .collect();
    // Rows read without any index: performance_schema files full scans under a NULL index name.
    let scans: HashMap<String, f64> = query_with(
        conn,
        "SELECT OBJECT_NAME AS name, COUNT_READ AS read_rows FROM performance_schema.table_io_waits_summary_by_index_usage
          WHERE OBJECT_SCHEMA = ? AND INDEX_NAME IS NULL",
        vec![Value::from(db)],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| Some((r.string("name").to_ascii_lowercase(), r.number("read_rows")?)))
    .collect();
    let analyzed: HashMap<String, String> = query_with(
        conn,
        "SELECT table_name AS name, DATE_FORMAT(last_update, '%Y-%m-%dT%H:%i:%sZ') AS analyzed
           FROM mysql.innodb_table_stats WHERE database_name = ?",
        vec![Value::from(db)],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| Some((r.string("name").to_ascii_lowercase(), r.text("analyzed")?)))
    .collect();
    Ok(rows
        .iter()
        .map(|r| {
            let name = r.string("name");
            let key = name.to_ascii_lowercase();
            let io = io.get(&key);
            let data = r.number("data_bytes");
            let index = r.number("index_bytes");
            TableStat {
                schema: db.to_string(),
                rows: r.number("row_estimate"),
                total_bytes: match (data, index) {
                    (None, None) => None,
                    (d, i) => Some(d.unwrap_or(0.0) + i.unwrap_or(0.0)),
                },
                data_bytes: data,
                index_bytes: index,
                has_primary_key: r.flag("has_pk"),
                is_heap: None,
                seq_scans: None,
                seq_rows_read: scans.get(&key).copied(),
                index_scans: None,
                inserts: io.and_then(|r| r.number("inserts")),
                updates: io.and_then(|r| r.number("updates")),
                deletes: io.and_then(|r| r.number("deletes")),
                dead_rows: None,
                modifications_since_analyze: None,
                last_vacuum: None,
                last_analyze: analyzed.get(&key).cloned(),
                forwarded_records: None,
                forwarded_fetches: None,
                xid_age: None,
                name,
                storage: None,
                in_row_bytes: None,
                lob_bytes: None,
                overflow_bytes: None,
                partition_of: None,
                counters_since: None,
            }
        })
        .collect())
}

async fn columns(conn: &mut Conn, db: &str) -> Result<Vec<ColumnInfo>> {
    let rows = query_with(
        conn,
        "SELECT TABLE_NAME AS table_name, COLUMN_NAME AS name, COLUMN_TYPE AS data_type, IS_NULLABLE AS nullable,
                COLUMN_DEFAULT AS default_value, EXTRA AS extra, ORDINAL_POSITION AS ordinal
           FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, ORDINAL_POSITION",
        vec![Value::from(db)],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: db.to_string(),
            table: r.string("table_name"),
            name: r.string("name"),
            data_type: r.string("data_type"),
            nullable: r.string("nullable").eq_ignore_ascii_case("YES"),
            default: r.text("default_value"),
            generated: r.string("extra").to_ascii_lowercase().contains("auto_increment"),
            ordinal: r.number("ordinal").unwrap_or(0.0) as u32,
        })
        .collect())
}

/// Indexes with their key columns, size and usage since the server started. The note explains missing usage counters.
async fn indexes(conn: &mut Conn, db: &str, probe: &Probe) -> Result<(Vec<IndexStat>, Option<String>)> {
    let with_visibility = "SELECT TABLE_NAME AS table_name, INDEX_NAME AS index_name, NON_UNIQUE AS non_unique,
                SEQ_IN_INDEX AS seq, COLUMN_NAME AS column_name, COLLATION AS collation, SUB_PART AS sub_part,
                INDEX_TYPE AS index_type, EXPRESSION AS expression, IS_VISIBLE AS visible
           FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX";
    let without_visibility = "SELECT TABLE_NAME AS table_name, INDEX_NAME AS index_name, NON_UNIQUE AS non_unique,
                SEQ_IN_INDEX AS seq, COLUMN_NAME AS column_name, COLLATION AS collation, SUB_PART AS sub_part,
                INDEX_TYPE AS index_type
           FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX";
    let rows = match query_with(conn, with_visibility, vec![Value::from(db)]).await {
        Ok(rows) => rows,
        Err(_) => query_with(conn, without_visibility, vec![Value::from(db)]).await?,
    };
    let engines: HashMap<String, String> = query_with(
        conn,
        "SELECT TABLE_NAME AS name, ENGINE AS engine FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await?
    .into_iter()
    .map(|r| (r.string("name").to_ascii_lowercase(), r.string("engine")))
    .collect();
    let page_size =
        query_one(conn, "SELECT @@innodb_page_size AS page").await.ok().flatten().and_then(|r| r.number("page"));
    let sizes: HashMap<(String, String), f64> = match page_size {
        Some(page) => query_with(
            conn,
            "SELECT table_name AS t, index_name AS i, stat_value AS pages FROM mysql.innodb_index_stats
              WHERE database_name = ? AND stat_name = 'size'",
            vec![Value::from(db)],
        )
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let table = r.string("t");
            let table = table.split("#p#").next().unwrap_or(&table).to_ascii_lowercase();
            Some(((table, r.string("i").to_ascii_lowercase()), r.number("pages")? * page))
        })
        .fold(HashMap::new(), |mut acc, (key, bytes)| {
            *acc.entry(key).or_insert(0.0) += bytes;
            acc
        }),
        None => HashMap::new(),
    };
    let (usage, usage_note) = if probe.performance_schema {
        match query_with(
            conn,
            "SELECT i.OBJECT_NAME AS t, i.INDEX_NAME AS i, i.COUNT_FETCH AS read_count,
                    t.COUNT_INSERT + t.COUNT_UPDATE + t.COUNT_DELETE AS write_count
               FROM performance_schema.table_io_waits_summary_by_index_usage i
               LEFT JOIN performance_schema.table_io_waits_summary_by_table t
                 ON t.OBJECT_TYPE = i.OBJECT_TYPE AND t.OBJECT_SCHEMA = i.OBJECT_SCHEMA AND t.OBJECT_NAME = i.OBJECT_NAME
              WHERE i.OBJECT_SCHEMA = ? AND i.INDEX_NAME IS NOT NULL",
            vec![Value::from(db)],
        )
        .await
        {
            Ok(rows) => (
                rows.into_iter()
                    .map(|r| {
                        let key = (r.string("t").to_ascii_lowercase(), r.string("i").to_ascii_lowercase());
                        (key, (r.number("read_count"), r.number("write_count")))
                    })
                    .collect::<HashMap<_, _>>(),
                None,
            ),
            Err(e) => (HashMap::new(), Some(format!("Reading index usage failed: {}", e.message))),
        }
    } else if probe.userstat == Some(true) {
        match userstat_usage(conn, db, &rows).await {
            Ok(usage) => (usage, None),
            Err(e) => (HashMap::new(), Some(format!("Reading userstat index statistics failed: {}", e.message))),
        }
    } else {
        (HashMap::new(), None)
    };

    let mut grouped: BTreeMap<(String, String), Vec<&Rec>> = BTreeMap::new();
    for r in &rows {
        grouped.entry((r.string("table_name"), r.string("index_name"))).or_default().push(r);
    }
    let mut out = Vec::new();
    for ((table, name), parts) in grouped {
        let first = parts[0];
        let columns: Vec<String> = parts
            .iter()
            .map(|p| {
                let base = match (p.text("column_name"), p.text("expression")) {
                    (Some(c), _) => c,
                    (None, Some(e)) => format!("({e})"),
                    (None, None) => "?".into(),
                };
                let base = match p.number("sub_part") {
                    Some(n) => format!("{base}({})", n as u64),
                    None => base,
                };
                if p.string("collation") == "D" { format!("{base} DESC") } else { base }
            })
            .collect();
        let primary = name.eq_ignore_ascii_case("PRIMARY");
        let unique = first.number("non_unique") == Some(0.0);
        let key = (table.to_ascii_lowercase(), name.to_ascii_lowercase());
        let used = usage.get(&key);
        let innodb = engines.get(&key.0).is_some_and(|e| e.eq_ignore_ascii_case("InnoDB"));
        let quoted = |s: &str| format!("`{}`", s.replace('`', "``"));
        let definition = if primary {
            format!("ALTER TABLE {}.{} ADD PRIMARY KEY ({})", quoted(db), quoted(&table), columns.join(", "))
        } else {
            format!(
                "CREATE {}INDEX {} ON {}.{} ({})",
                if unique { "UNIQUE " } else { "" },
                quoted(&name),
                quoted(db),
                quoted(&table),
                columns.join(", ")
            )
        };
        out.push(IndexStat {
            schema: db.to_string(),
            kind: first.string("index_type").to_ascii_lowercase(),
            include: Vec::new(),
            filter: None,
            is_unique: unique,
            is_primary_key: primary,
            is_clustered: primary && innodb,
            is_disabled: first.text("visible").is_some_and(|v| v.eq_ignore_ascii_case("NO")),
            size_bytes: sizes.get(&key).copied(),
            // No row means the table hasn't been opened since the server started: usage is unknown, not zero.
            reads: used.and_then(|u| u.0),
            // Writes to the table the index is kept up for: performance_schema files every insert under no index, and
            // counts an update or delete under the index that found its rows, so per-index counts aren't its upkeep.
            writes: used.and_then(|u| u.1),
            seeks: None,
            scans: None,
            lookups: None,
            last_read: None,
            definition: Some(definition),
            table,
            name,
            columns,
            contention: None,
            created_at_latest: None,
            partition_of: None,
            opclasses: None,
            counters_since: None,
        });
    }
    Ok((out, usage_note))
}

/// Index reads from MariaDB's userstat counters (since `userstat` was turned on or last flushed). An index missing from
/// INDEX_STATISTICS on a table that has been read was not used; on a table nobody has touched, usage is unknown.
async fn userstat_usage(
    conn: &mut Conn,
    db: &str,
    statistics: &[Rec],
) -> Result<HashMap<(String, String), (Option<f64>, Option<f64>)>> {
    let tables: HashMap<String, f64> = query_with(
        conn,
        "SELECT TABLE_NAME AS t, ROWS_CHANGED AS changed FROM information_schema.TABLE_STATISTICS WHERE TABLE_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await?
    .into_iter()
    .map(|r| (r.string("t").to_ascii_lowercase(), r.number("changed").unwrap_or(0.0)))
    .collect();
    let read: HashMap<(String, String), f64> = query_with(
        conn,
        "SELECT TABLE_NAME AS t, INDEX_NAME AS i, ROWS_READ AS rows_read FROM information_schema.INDEX_STATISTICS WHERE TABLE_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await?
    .into_iter()
    .map(|r| ((r.string("t").to_ascii_lowercase(), r.string("i").to_ascii_lowercase()), r.number("rows_read").unwrap_or(0.0)))
    .collect();
    let mut out = HashMap::new();
    for r in statistics {
        let key = (r.string("table_name").to_ascii_lowercase(), r.string("index_name").to_ascii_lowercase());
        let Some(changed) = tables.get(&key.0) else { continue };
        let reads = read.get(&key).copied().unwrap_or(0.0);
        out.insert(key, (Some(reads), Some(*changed)));
    }
    Ok(out)
}

async fn foreign_keys(conn: &mut Conn, db: &str) -> Result<Vec<ForeignKey>> {
    let rows = query_with(
        conn,
        "SELECT CONSTRAINT_NAME AS name, TABLE_NAME AS table_name, COLUMN_NAME AS column_name,
                REFERENCED_TABLE_SCHEMA AS ref_schema, REFERENCED_TABLE_NAME AS ref_table, REFERENCED_COLUMN_NAME AS ref_column
           FROM information_schema.KEY_COLUMN_USAGE
          WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL
          ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
        vec![Value::from(db)],
    )
    .await?;
    let mut out: Vec<ForeignKey> = Vec::new();
    for r in &rows {
        let (name, table) = (r.string("name"), r.string("table_name"));
        match out.iter_mut().find(|f| f.name == name && f.table == table) {
            Some(fk) => {
                fk.columns.push(r.string("column_name"));
                fk.ref_columns.push(r.string("ref_column"));
            }
            None => out.push(ForeignKey {
                name,
                schema: db.to_string(),
                table,
                columns: vec![r.string("column_name")],
                ref_schema: r.string("ref_schema"),
                ref_table: r.string("ref_table"),
                ref_columns: vec![r.string("ref_column")],
            }),
        }
    }
    Ok(out)
}

/// Server variables the checks and the UI read, with their compiled-in defaults: MySQL 8.0/8.4, then MariaDB 10.11/11.x
/// (`None` where that server has no such variable).
const SETTINGS: [(&str, Option<&str>, Option<&str>); 21] = [
    ("performance_schema_digests_size", Some("10000"), Some("-1")),
    ("performance_schema_max_digest_length", Some("1024"), Some("1024")),
    ("performance_schema_max_sql_text_length", Some("1024"), Some("1024")),
    ("max_digest_length", Some("1024"), Some("1024")),
    ("innodb_buffer_pool_size", Some("134217728"), Some("134217728")),
    ("innodb_page_size", Some("16384"), Some("16384")),
    ("tmp_table_size", Some("16777216"), Some("16777216")),
    ("max_heap_table_size", Some("16777216"), Some("16777216")),
    ("sort_buffer_size", Some("262144"), Some("2097152")),
    ("join_buffer_size", Some("262144"), Some("262144")),
    ("long_query_time", Some("10.000000"), Some("10.000000")),
    ("slow_query_log", Some("OFF"), Some("OFF")),
    ("log_output", Some("FILE"), Some("FILE")),
    ("performance_schema", Some("ON"), Some("OFF")),
    ("max_connections", Some("151"), Some("151")),
    ("innodb_flush_log_at_trx_commit", Some("1"), Some("1")),
    ("transaction_isolation", Some("REPEATABLE-READ"), Some("REPEATABLE-READ")),
    ("tx_isolation", None, Some("REPEATABLE-READ")),
    ("max_execution_time", Some("0"), None),
    ("max_statement_time", None, Some("0.000000")),
    ("userstat", None, Some("OFF")),
];

/// "replica" when this server replicates from another: its replication status has a row, or, where this login can't
/// read that status, it is read-only (how replicas and Aurora's readers run). Its usage counters count the reads run on
/// it (and the replication applier's), not the primary's workload.
async fn replica_role(conn: &mut Conn, probe: &Probe) -> Option<&'static str> {
    let status = match query(conn, "SHOW REPLICA STATUS").await {
        Ok(rows) => Ok(rows),
        Err(_) => query(conn, "SHOW SLAVE STATUS").await,
    };
    match status {
        Ok(rows) if !rows.is_empty() => Some("replica"),
        Ok(_) => None,
        Err(_) => probe.read_only.then_some("replica"),
    }
}

async fn settings(conn: &mut Conn, flavor: Flavor) -> Result<Vec<Setting>> {
    let default_of = |name: &str| {
        SETTINGS.iter().find(|(n, _, _)| *n == name).and_then(|(_, mysql, maria)| match flavor {
            Flavor::Mysql => *mysql,
            Flavor::Mariadb => *maria,
        })
    };
    let names: Vec<&str> = SETTINGS.iter().filter(|(n, _, _)| default_of(n).is_some()).map(|(n, _, _)| *n).collect();
    let rows = vars::variables(conn, &names).await?;
    Ok(rows
        .into_iter()
        .map(|(name, value)| {
            let default = default_of(&name).map(str::to_string);
            Setting { name, value, default_value: default, scope: "server".into() }
        })
        .chain(statement_status(conn).await)
        .collect())
}

/// Whether performance_schema keeps up with the workload: digests it had no room for, statements lumped into the
/// overflow row, and the consumer that has to be on for digests at all. Best effort; each part may be unreadable.
async fn statement_status(conn: &mut Conn) -> Vec<Setting> {
    let mut out = Vec::new();
    let status =
        |name: &str, value: String| Setting { name: name.into(), value, default_value: None, scope: "status".into() };
    if let Ok(found) = vars::status(conn, &["Performance_schema_digest_lost"]).await
        && let Some(lost) = found.get("Performance_schema_digest_lost")
    {
        out.push(status("performance_schema_digest_lost", lost.to_string()));
    }
    if let Ok(Some(r)) = query_one(
        conn,
        "SELECT COUNT_STAR AS calls, SUM_TIMER_WAIT / 1000000000 AS total_ms
           FROM performance_schema.events_statements_summary_by_digest WHERE DIGEST IS NULL",
    )
    .await
    {
        out.push(status("performance_schema.untracked_statements", r.string("calls")));
        out.push(status("performance_schema.untracked_ms", r.string("total_ms")));
    }
    if let Ok(rows) = query(
        conn,
        "SELECT NAME AS name, ENABLED AS enabled FROM performance_schema.setup_consumers
          WHERE NAME IN ('statements_digest', 'events_statements_current', 'global_instrumentation')",
    )
    .await
    {
        for r in rows {
            out.push(status(&format!("performance_schema.consumer.{}", r.string("name")), r.string("enabled")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(id: &str, time: f64, calls: f64, examined: f64) -> Digest {
        Digest {
            stat: QueryStat {
                id: id.into(),
                query_hash: None,
                text: String::new(),
                parameters: None,
                object_name: None,
                executions: calls,
                total_time_ms: time,
                mean_time_ms: 0.0,
                max_time_ms: None,
                stddev_time_ms: None,
                cpu_time_ms: None,
                rows: None,
                logical_reads: Some(examined),
                physical_reads: None,
                temp_writes: None,
                first_seen: None,
                last_seen: None,
                plans: Vec::new(),
                plan: None,
                waits: Vec::new(),
                trend: None,
                spread: None,
                costs: None,
                rows_affected: None,
                plan_id: None,
            },
            sample: None,
            sample_from: SampleFrom::Digest,
            rows_examined: examined,
        }
    }

    fn probe(flavor: Flavor, performance_schema: bool) -> Probe {
        Probe {
            version: "10.11.6-MariaDB".into(),
            comment: String::new(),
            database: "shop".into(),
            size_bytes: None,
            started_at: None,
            now: "2026-10-11T12:00:00Z".into(),
            user: "app@%".into(),
            cloud: None,
            performance_schema,
            flavor,
            log_output: "FILE".into(),
            slow_query_log: false,
            long_query_time: Some(10.0),
            userstat: (flavor == Flavor::Mariadb).then_some(false),
            read_only: false,
        }
    }

    #[test]
    fn a_plan_says_which_run_it_was_planned_from() {
        // performance_schema keeps a digest's slowest recent run as its sample: its values may be an outlier's.
        assert!(SampleFrom::Digest.sentence().contains("slowest recent run"));
        assert!(SampleFrom::SlowLog.sentence().contains("a slow run"));
        assert!(!SampleFrom::History.sentence().contains("outlier"));
    }

    #[test]
    fn without_performance_schema_the_no_restart_path_comes_first() {
        let p = probe(Flavor::Mariadb, false);
        assert_eq!(
            p.unlock_statement().as_deref(),
            Some(
                "SET GLOBAL userstat = ON, GLOBAL slow_query_log = ON, GLOBAL log_output = 'FILE,TABLE', GLOBAL long_query_time = 0.1;"
            )
        );
        let info = p.server_info();
        assert_eq!(info.product, "MariaDB 10.11.6");
        let ps = info.missing.iter().find(|m| m.capability == Capability::PerformanceSchema).unwrap();
        let fix = ps.fix.as_deref().unwrap();
        assert!(fix.starts_with("Run SET GLOBAL userstat = ON"), "{fix}");
        assert!(fix.contains("no restart needed") && fix.ends_with("restart MariaDB."), "{fix}");
        // Once the log goes to the table, only the restart remains.
        let logging = Probe {
            slow_query_log: true,
            log_output: "FILE,TABLE".into(),
            long_query_time: Some(0.1),
            userstat: Some(true),
            ..p
        };
        assert_eq!(logging.unlock_statement(), None);
        let info = logging.server_info();
        assert!(info.has(Capability::SlowLog) && info.has(Capability::QueryPlans) && info.has(Capability::IndexUsage));
        let mysql = probe(Flavor::Mysql, false);
        assert!(!mysql.unlock_statement().unwrap().contains("userstat"), "MySQL has no userstat");
    }

    #[test]
    fn plan_notes_tell_missing_text_from_writes() {
        let outcome = PlanOutcome { planned: 2, no_text: 3, not_reads: 4, ..Default::default() };
        assert_eq!(outcome.note().as_deref(), Some("Planned 2 queries; 3 had no recorded statement text."));
        assert_eq!(PlanOutcome { planned: 5, not_reads: 2, ..Default::default() }.note(), None);
        assert!(reads_shape(" (SELECT 1)") && !reads_shape("UPDATE t SET a = 1"));
    }

    #[test]
    fn keeps_chatty_and_scan_heavy_digests() {
        let mut all: Vec<Digest> = (0..20).map(|i| digest(&format!("t{i}"), 1000.0 - f64::from(i), 1.0, 1.0)).collect();
        all.push(digest("chatty", 1.0, 1e6, 1.0));
        all.push(digest("scanner", 2.0, 1.0, 1e9));
        let kept = select_top(all, 8);
        let ids: Vec<&str> = kept.iter().map(|d| d.stat.id.as_str()).collect();
        assert_eq!(kept.len(), 8);
        assert!(ids.contains(&"chatty") && ids.contains(&"scanner") && ids.contains(&"t0"));
        assert_eq!(ids[0], "t0", "kept digests are ordered by time");
    }

    #[test]
    fn sample_plans_keep_no_literal_values() {
        let json = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"1.20"},"table":{"table_name":"customers","access_type":"ALL","rows_examined_per_scan":100,"rows_produced_per_join":10,"filtered":"10.00","cost_info":{"read_cost":"1.00","eval_cost":"0.20","prefix_cost":"1.20","data_read_per_join":"1K"},"used_columns":["id","email"],"attached_condition":"((`shop`.`customers`.`email` = 'alice@example.com') and (`shop`.`customers`.`note` like '%vip%'))"}}}"#;
        let sample = "SELECT id FROM customers WHERE email = 'alice@example.com' AND note LIKE '%vip%'";
        let plan =
            sample_plan(json, sample, "shop", "SELECT `id` FROM `customers` WHERE `email` = ? AND `note` LIKE ?")
                .unwrap();
        let text = serde_json::to_string(&plan).unwrap();
        assert!(!text.contains("alice"), "{text}");
        assert!(!text.contains("vip"), "{text}");
        assert!(text.contains("'%?%'"), "the LIKE pattern keeps its shape: {text}");
        assert_eq!(plan.statement.as_deref(), Some("SELECT `id` FROM `customers` WHERE `email` = ? AND `note` LIKE ?"));
    }

    #[test]
    fn samples_that_matched_nothing_are_explained() {
        let json = r#"{"query_block":{"select_id":1,"message":"no matching row in const table"}}"#;
        let plan =
            sample_plan(json, "SELECT * FROM orders WHERE id = 0", "shop", "SELECT * FROM `orders` WHERE `id` = ?")
                .unwrap();
        assert!(plan.root.children.is_empty(), "the parser keeps the empty block; attach_plans skips it");
        let outcome = PlanOutcome { planned: 2, empty: 1, ..Default::default() };
        let note = outcome.note().unwrap();
        assert!(note.contains("matched no rows"), "{note}");
    }

    #[test]
    fn samples_that_cannot_be_planned_are_skipped() {
        assert!(plannable("SELECT id FROM orders WHERE id = 7"));
        assert!(plannable("  (SELECT 1) UNION (SELECT 2)"));
        assert!(!plannable("SELECT id FROM orders WHERE id = ?"));
        assert!(!plannable("SELECT a, b, c FROM t WHERE x IN (1, 2, ..."));
        assert!(!plannable("UPDATE orders SET x = 1"));
    }

    #[test]
    fn windows_are_measured_in_hours() {
        assert_eq!(hours_between("2026-10-11T06:00:00Z", "2026-10-11T07:30:00Z"), Some(1.5));
        assert_eq!(crate::pressure::category("wait/io/file/innodb/innodb_data_file"), "IO");
        assert_eq!(crate::pressure::category("wait/lock/metadata/sql/mdl"), "Lock");
    }

    #[test]
    fn histogram_percentiles_and_folding() {
        let buckets = [[0.0, 1.0, 50.0], [1.0, 2.0, 45.0], [2.0, 4.0, 4.0], [500.0, 1000.0, 1.0]];
        assert_eq!(quantile(&buckets, 0.5), Some(1.0));
        assert_eq!(quantile(&buckets, 0.95), Some(2.0));
        assert_eq!(quantile(&buckets, 0.995), Some(1000.0));
        assert_eq!(fold_buckets(&buckets, 2), vec![[0.0, 2.0, 95.0], [2.0, 1000.0, 5.0]]);
    }
}
