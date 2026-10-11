use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use culprit_core::{
    Capability, CollectorNote, ColumnInfo, ConnectionSpec, Engine, Error, ForeignKey, IndexStat, MissingCapability,
    PlanKind, QueryStat, Result, SNAPSHOT_FORMAT, ServerInfo, Setting, Snapshot, SnapshotOptions, Spread, TableStat,
    WaitStat, Workload,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Value};

use crate::connect::{Connector, Scope, TAG};
use crate::plan_json;
use crate::rows::{Rec, explain_one, query, query_one, query_with, tagged};

const PS_GRANT: &str = "GRANT SELECT ON performance_schema.* TO";
const PS_ENABLE: &str = "Set performance_schema = ON in the server configuration and restart MySQL.";

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
}

impl Probe {
    pub fn product(&self) -> String {
        let short = self.version.split('-').next().unwrap_or(&self.version);
        if self.version.to_ascii_lowercase().contains("mariadb") {
            return format!("MariaDB {short}");
        }
        format!("MySQL {short}")
    }

    fn grant_target(&self) -> String {
        match self.user.split_once('@') {
            Some((user, host)) => format!("'{}'@'{}'", user.replace('\'', "''"), host.replace('\'', "''")),
            None => format!("'{}'", self.user.replace('\'', "''")),
        }
    }

    pub fn server_info(&self) -> ServerInfo {
        let mut capabilities = vec![Capability::TableStats, Capability::Sessions, Capability::Showplan];
        let mut missing = Vec::new();
        if self.performance_schema {
            capabilities.extend([Capability::PerformanceSchema, Capability::IndexUsage, Capability::QueryPlans]);
        } else {
            let reason =
                "performance_schema is off, so MySQL keeps no statement or index usage statistics.".to_string();
            for capability in [Capability::PerformanceSchema, Capability::IndexUsage, Capability::QueryPlans] {
                missing.push(MissingCapability { capability, reason: reason.clone(), fix: Some(PS_ENABLE.into()) });
            }
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
        Error::invalid("A MySQL connection needs a database (schema) to analyze")
            .with_hint("Add it to the connection string, e.g. mysql://user@host:3306/shop")
    })
}

pub async fn probe(conn: &mut Conn, connector: &Connector) -> Result<Probe> {
    let database = database_of(connector)?;
    let row = query_one(
        conn,
        "SELECT VERSION() AS version, @@version_comment AS comment, @@basedir AS basedir, CURRENT_USER() AS user_name,
                @@performance_schema AS ps, DATE_FORMAT(UTC_TIMESTAMP(), '%Y-%m-%dT%H:%i:%sZ') AS now",
    )
    .await?
    .ok_or_else(|| Error::internal("MySQL returned no server information"))?;
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
    let uptime = query_one(
        conn,
        "SELECT VARIABLE_VALUE AS uptime FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'",
    )
    .await
    .ok()
    .flatten()
    .and_then(|r| r.number("uptime"));
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
    let cloud = detect_cloud(conn, &connector.host, &row.string("basedir"), &comment).await;
    Ok(Probe {
        version,
        comment,
        database,
        size_bytes: size.and_then(|r| r.number("size")),
        started_at,
        now: row.string("now"),
        user: row.string("user_name"),
        cloud,
        performance_schema: row.flag("ps"),
    })
}

async fn detect_cloud(conn: &mut Conn, host: &str, basedir: &str, comment: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    if host.ends_with(".mysql.database.azure.com") {
        return Some("Azure Database for MySQL".into());
    }
    let aurora = query_one(conn, "SELECT AURORA_VERSION() AS v").await.ok().flatten().is_some();
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
            let settings = settings(&mut conn).await;
            let waits = crate::pressure::waits(&mut conn).await;
            let pressure = crate::pressure::collect(&mut conn).await;
            Ok::<_, Error>((settings, waits, pressure))
        },
    );

    let mut server = probe.server_info();
    let mut notes = Vec::new();
    let mut note =
        |collector: &str, message: String| notes.push(CollectorNote { collector: collector.into(), message });

    let workload = match workload {
        Ok(Some(collected)) => {
            if let Some(message) = collected.plan_note {
                note("plans", message);
            }
            if collected.workload.queries.iter().all(|q| q.plan.is_none()) {
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
        }
        Err(e) => note("settings", e.message),
    }

    Ok(Snapshot {
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
    })
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
    plan_note: Option<String>,
}

struct Digest {
    stat: QueryStat,
    sample: Option<String>,
    rows_examined: f64,
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
        return Ok(None);
    }
    let available = digest_columns(conn).await?;
    let has = |c: &str| available.iter().any(|a| a == c);
    let optional = |c: &str| if has(c) { c.to_string() } else { format!("NULL AS {c}") };
    // Rows examined stands in for "logical reads": MySQL counts rows the storage engine handed to the server, not pages.
    // EXPLAINs are Culprit's own planning; `SELECT @@…` digests are driver handshakes (Culprit's included), never actionable.
    let sql = format!(
        "SELECT DIGEST AS digest, DIGEST_TEXT AS digest_text, COUNT_STAR AS calls, SUM_TIMER_WAIT AS total_ps,
                MAX_TIMER_WAIT AS max_ps, MIN_TIMER_WAIT AS min_ps, SUM_ROWS_SENT AS rows_sent, SUM_ROWS_EXAMINED AS rows_examined,
                SUM_CREATED_TMP_DISK_TABLES AS tmp_disk, SUM_NO_INDEX_USED AS no_index, SUM_LOCK_TIME AS lock_ps,
                {cpu}, {sample}, {p95}, {p99},
                DATE_FORMAT(FIRST_SEEN, '%Y-%m-%dT%H:%i:%sZ') AS first_seen,
                DATE_FORMAT(LAST_SEEN, '%Y-%m-%dT%H:%i:%sZ') AS last_seen
           FROM performance_schema.events_statements_summary_by_digest
          WHERE SCHEMA_NAME = ? AND DIGEST IS NOT NULL AND DIGEST_TEXT NOT LIKE 'EXPLAIN %'
            AND DIGEST_TEXT NOT LIKE 'SELECT @@%'",
        cpu = optional("SUM_CPU_TIME"),
        sample = optional("QUERY_SAMPLE_TEXT"),
        p95 = optional("QUANTILE_95"),
        p99 = optional("QUANTILE_99"),
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
    let mut plan_note = None;
    if !kept.is_empty() && options.plans > 0 {
        plan_note = attach_plans(connector, &probe.database, &mut kept, options.plans as usize).await;
    }
    let queries = kept.into_iter().map(|d| d.stat).collect();
    Ok(Some(CollectedWorkload {
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
        },
        plan_note,
    }))
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
        },
        sample: r.text("query_sample_text"),
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

async fn attach_plans(connector: &Connector, database: &str, queries: &mut [Digest], limit: usize) -> Option<String> {
    let mut conn = match connector.connect(Scope::Database).await {
        Ok(conn) => conn,
        Err(e) => return Some(format!("Could not open a session to plan statements: {}", e.message)),
    };
    let _ = conn.query_drop(tagged("SET SESSION explain_json_format_version = 1")).await;
    let (mut planned, mut skipped, mut failed) = (0, 0, 0);
    for digest in queries.iter_mut().take(limit) {
        // Sample text was written by whoever ran the statement: it must lex as exactly one statement, and EXPLAIN goes
        // through a prepared statement, which can't hold a second one.
        let sample = digest.sample.as_deref().filter(|s| plannable(s));
        let Some(sample) = sample.and_then(|s| culprit_core::sql::single_statement(s, Engine::Mysql).ok()) else {
            skipped += 1;
            continue;
        };
        let sql = format!("EXPLAIN FORMAT=JSON {sample}");
        match explain_one(&mut conn, &sql).await {
            Ok(Some(row)) => {
                let json = row.text("explain").or_else(|| row.text("EXPLAIN")).unwrap_or_default();
                match plan_json::parse(&json, PlanKind::Estimated, Some(sample), Some(database)) {
                    Ok(mut plan) => {
                        plan.statement = Some(digest.stat.text.clone());
                        digest.stat.plan = Some(plan);
                        planned += 1;
                    }
                    Err(_) => failed += 1,
                }
            }
            _ => failed += 1,
        }
    }
    let _ = conn.disconnect().await;
    (skipped + failed > 0).then(|| {
        format!(
            "Planned {planned} statement{}; {skipped} had no usable sample text (truncated or server-side prepared) and {failed} could not be explained.",
            if planned == 1 { "" } else { "s" }
        )
    })
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
            "SELECT OBJECT_NAME AS t, INDEX_NAME AS i, COUNT_FETCH AS read_count, COUNT_INSERT + COUNT_UPDATE + COUNT_DELETE AS write_count
               FROM performance_schema.table_io_waits_summary_by_index_usage
              WHERE OBJECT_SCHEMA = ? AND INDEX_NAME IS NOT NULL",
            vec![Value::from(db)],
        )
        .await
        {
            Ok(rows) => (
                rows.into_iter()
                    .map(|r| ((r.string("t").to_ascii_lowercase(), r.string("i").to_ascii_lowercase()), r))
                    .collect::<HashMap<_, _>>(),
                None,
            ),
            Err(e) => (HashMap::new(), Some(format!("Reading index usage failed: {}", e.message))),
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
            reads: used.and_then(|r| r.number("read_count")),
            writes: used.and_then(|r| r.number("write_count")),
            seeks: None,
            scans: None,
            lookups: None,
            last_read: None,
            definition: Some(definition),
            table,
            name,
            columns,
        });
    }
    Ok((out, usage_note))
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

/// Server variables the checks and the UI read, with their compiled-in defaults (MySQL 8.0/8.4).
const SETTINGS: [(&str, &str); 16] = [
    ("performance_schema_digests_size", "10000"),
    ("performance_schema_max_digest_length", "1024"),
    ("performance_schema_max_sql_text_length", "1024"),
    ("max_digest_length", "1024"),
    ("innodb_buffer_pool_size", "134217728"),
    ("innodb_page_size", "16384"),
    ("tmp_table_size", "16777216"),
    ("max_heap_table_size", "16777216"),
    ("sort_buffer_size", "262144"),
    ("join_buffer_size", "262144"),
    ("long_query_time", "10.000000"),
    ("slow_query_log", "OFF"),
    ("performance_schema", "ON"),
    ("max_connections", "151"),
    ("innodb_flush_log_at_trx_commit", "1"),
    ("transaction_isolation", "REPEATABLE-READ"),
];

async fn settings(conn: &mut Conn) -> Result<Vec<Setting>> {
    let names = SETTINGS.iter().map(|(n, _)| format!("'{n}'")).collect::<Vec<_>>().join(", ");
    let rows = query(
        conn,
        &format!("SELECT VARIABLE_NAME AS name, VARIABLE_VALUE AS value FROM performance_schema.global_variables WHERE VARIABLE_NAME IN ({names})"),
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let name = r.string("name").to_ascii_lowercase();
            let default = SETTINGS.iter().find(|(n, _)| *n == name).map(|(_, d)| d.to_string());
            Setting { name, value: r.string("value"), default_value: default, scope: "server".into() }
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
    if let Ok(Some(r)) = query_one(
        conn,
        "SELECT VARIABLE_VALUE AS lost FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Performance_schema_digest_lost'",
    )
    .await
    {
        out.push(status("performance_schema_digest_lost", r.string("lost")));
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
            },
            sample: None,
            rows_examined: examined,
        }
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
