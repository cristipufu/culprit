//! Statement digests, slow-log fallback, samples and safe estimated plan capture.
use crate::{
    connect::{Connector, Flavor, Scope, TAG},
    fingerprint::{fingerprint, matches_digest},
    plan_json,
    probe::Probe,
    rows::{Rec, explain_one, query, query_one, query_with, tagged},
    slowlog,
};
use culprit_core::{Engine, Plan, PlanKind, QueryStat, Result, SnapshotOptions, Spread, WaitStat, Workload};
use mysql_async::{Conn, Value, prelude::Queryable};
use std::collections::HashMap;

pub(crate) struct CollectedWorkload {
    pub(crate) workload: Workload,
    /// Waits the workload source measured itself (the slow query log's lock time), for servers without wait totals.
    pub(crate) waits: Vec<WaitStat>,
    pub(crate) notes: Vec<(&'static str, String)>,
    /// Why no statement could be planned and what would change that, when none was.
    pub(crate) no_plans: Option<(String, Option<String>)>,
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

pub(crate) async fn collect_workload(
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
            counter_set: None,
            history_inputs: Vec::new(),
            measurement: None,
            source: "performance_schema".into(),
            kind: Some(culprit_core::WorkloadKind::PerformanceSchema),
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
            hourly_time_ms: None,
            min_logged_ms: None,
            log: None,
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
            plan_history: None,
            resolved_write_target: None,
            requests: None,
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
                            plan.facts_mut().server_version = Some(version.to_string());
                            plan.props.insert("server_version".into(), version.to_string());
                        }
                        // A simple MariaDB plan reads like MySQL's JSON; say which server made it so notes name it.
                        if connector.flavor() == Flavor::Mariadb {
                            plan.facts_mut().flavor = Some("MariaDB".into());
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
/// keep their wildcard shape (`'%?'`), so a leading wildcard is still visible. One number is kept: the row count of
/// the sample's LIMIT where the digest writes `LIMIT ?` ([`culprit_core::insights::LIMIT_ROWS`]), the statement's
/// shape rather than its data, so rows returned can be weighed against it.
fn sample_plan(json: &str, sample: &str, database: &str, digest_text: &str) -> Result<Plan> {
    let plan = plan_json::parse(json, PlanKind::Estimated, Some(sample), Some(database))?;
    let mut clean = culprit_core::redact::redact_plan(&plan);
    clean.raw = plan.raw.as_deref().map(|raw| culprit_core::redact::redact_native(raw, Engine::Mysql).into());
    clean.statement = Some(digest_text.to_string());
    if culprit_core::sql::has_limit(digest_text, Engine::Mysql)
        && culprit_core::sql::limit_count(digest_text, Engine::Mysql).is_none()
        && let Some(rows) = culprit_core::sql::limit_count(sample, Engine::Mysql)
    {
        clean.props.insert(culprit_core::insights::LIMIT_ROWS.into(), rows.to_string());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(id: &str, time: f64, calls: f64, examined: f64) -> Digest {
        Digest {
            stat: QueryStat {
                plan_history: None,
                resolved_write_target: None,
                requests: None,
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

    #[test]
    fn a_plan_says_which_run_it_was_planned_from() {
        // performance_schema keeps a digest's slowest recent run as its sample: its values may be an outlier's.
        assert!(SampleFrom::Digest.sentence().contains("slowest recent run"));
        assert!(SampleFrom::SlowLog.sentence().contains("a slow run"));
        assert!(!SampleFrom::History.sentence().contains("outlier"));
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
        assert!(!plan.props.contains_key(culprit_core::insights::LIMIT_ROWS), "no LIMIT, no count");
    }

    #[test]
    fn a_sample_keeps_the_count_its_digests_limit_hides() {
        let json = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"50000.20"},"table":{"table_name":"orders","access_type":"ALL","rows_examined_per_scan":500000,"rows_produced_per_join":50000,"filtered":"10.00","cost_info":{"read_cost":"45000.00","eval_cost":"5000.20","prefix_cost":"50000.20","data_read_per_join":"106M"},"used_columns":["id","promo_code"],"attached_condition":"(`shop`.`orders`.`promo_code` = 'SPRING-7')"}}}"#;
        let sample = "SELECT id FROM orders WHERE promo_code = 'SPRING-7' LIMIT 50";
        let digest = "SELECT `id` FROM `orders` WHERE `promo_code` = ? LIMIT ?";
        let plan = sample_plan(json, sample, "shop", digest).unwrap();
        assert_eq!(plan.props.get(culprit_core::insights::LIMIT_ROWS).map(String::as_str), Some("50"));
        // Redacting the plan again (an export, a share) keeps it: it is a number, not a value from the data.
        let again = culprit_core::redact::redact_plan(&plan);
        assert_eq!(again.props.get(culprit_core::insights::LIMIT_ROWS).map(String::as_str), Some("50"));
        assert!(!serde_json::to_string(&again).unwrap().contains("SPRING"));
        // A re-read of the saved plan keeps it too.
        let reread = plan_json::reparse(&plan, Some("shop")).unwrap();
        assert_eq!(reread.props.get(culprit_core::insights::LIMIT_ROWS).map(String::as_str), Some("50"));
        // A LIMIT written inside the sample's literal is the application's data, not the statement's shape: it never
        // reaches the plan, which redaction would otherwise leave alone.
        let note = "SELECT id FROM orders WHERE promo_code = 'LIMIT 4242' LIMIT ?";
        let plan = sample_plan(json, note, "shop", digest).unwrap();
        assert!(!plan.props.contains_key(culprit_core::insights::LIMIT_ROWS), "{:?}", plan.props);
        assert!(!serde_json::to_string(&plan).unwrap().contains("4242"));
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
