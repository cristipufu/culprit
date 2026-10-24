//! Cloud metrics for a connection, read through the person's own CLI sign-in: Amazon CloudWatch with `aws`, Azure Monitor
//! with `az`. Only metadata and metrics are read (`rds:Describe*`, `cloudwatch:GetMetricData`, Resource Graph,
//! `monitor metrics list`); Culprit stores no cloud credentials. Results are cached next to the connection's reports for
//! a few minutes, so the app, the CLI and agents share one fetch.

use std::path::{Path, PathBuf};
use std::time::Duration;

use culprit_core::changes::{epoch, short_time};
use culprit_core::cloud::{
    CloudPoint, CloudProvider, CloudSeries, CloudSignals, CloudUnit, Rule, ThrottleEpisode, ThrottleKind, episodes,
    gp2_baseline_iops, rds_cpu_baseline, rds_vcpus, sum_series, summarize,
};
use culprit_core::{Engine, Error, ErrorCode, Result, format};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ConnectionInfo, aws, discover, private_fs};

/// Metrics fetched within this long are reused.
const FRESH_SECONDS: f64 = 300.0;
/// What a resource is (class, storage, tier) changes rarely; look it up again after a day.
const RESOLVE_SECONDS: f64 = 86_400.0;
/// CloudWatch keeps 1-minute points for 15 days; never ask for more than 14.
const MAX_DAYS: f64 = 30.0;
const CLI_TIMEOUT: Duration = Duration::from_secs(25);

/// Where a connection lives in the cloud, from its host name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Target {
    /// An RDS instance endpoint, or an Aurora cluster endpoint (`reader` for `cluster-ro-`).
    Rds {
        region: String,
        id: String,
        cluster: bool,
        reader: bool,
    },
    AzureSql {
        server: String,
        database: String,
    },
    AzureFlexible {
        engine: Engine,
        server: String,
    },
}

/// The connection's cloud resource, when its host is one Culprit can read metrics for.
pub fn target(info: &ConnectionInfo) -> Option<Target> {
    let host = info.host.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let first = labels.first().copied().unwrap_or_default();
    if culprit_core::aws_iam_host_allowed(&host) {
        let region = aws::region_from_host(&host)?;
        let second = labels.get(1).copied().unwrap_or_default();
        if second.starts_with("proxy-") || !valid_rds_id(first) {
            return None;
        }
        let cluster = second.starts_with("cluster-");
        return Some(Target::Rds { region, id: first.to_string(), cluster, reader: second.starts_with("cluster-ro-") });
    }
    if !culprit_core::entra_host_allowed(info.engine, &host) || !valid_azure_name(first) {
        return None;
    }
    match info.engine {
        Engine::Mssql if host.contains(".database.") => {
            let database = info.database.clone().filter(|d| valid_azure_database(d))?;
            Some(Target::AzureSql { server: first.to_string(), database })
        }
        Engine::Postgres | Engine::Mysql => {
            Some(Target::AzureFlexible { engine: info.engine, server: first.to_string() })
        }
        _ => None,
    }
}

fn valid_rds_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id.starts_with(|c: char| c.is_ascii_alphabetic())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_azure_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Database names are embedded in a Resource Graph query: letters, digits, `_`, `-` and `.` only.
fn valid_azure_database(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// What the cloud says the resource is.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct Resource {
    service: String,
    name: String,
    /// RDS: region and instance id. Azure: the resource id.
    region: Option<String>,
    instance: Option<String>,
    azure_id: Option<String>,
    class: Option<String>,
    storage_type: Option<String>,
    allocated_gb: Option<f64>,
    iops: Option<f64>,
    insights: bool,
    sku: Option<String>,
    sku_tier: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    host: String,
    resource: Option<Resource>,
    resolved_at: Option<f64>,
    /// Recent fetches at each resolution, assessed for their original period.
    #[serde(default)]
    fetches: Vec<Fetch>,
}

#[derive(Serialize, Deserialize)]
struct Fetch {
    signals: CloudSignals,
    at: f64,
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("cloud.json")
}

fn read_cache(dir: &Path, host: &str) -> Cache {
    std::fs::read(cache_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .filter(|c| c.host == host)
        .unwrap_or_else(|| Cache { host: host.to_string(), ..Default::default() })
}

fn write_cache(dir: &Path, cache: &Cache) {
    if private_fs::create_dir(dir).is_ok()
        && let Ok(json) = serde_json::to_vec(cache)
    {
        let _ = private_fs::write_atomic(&cache_path(dir), &json);
    }
}

fn now() -> f64 {
    chrono::Utc::now().timestamp() as f64
}

fn iso(seconds: f64) -> String {
    chrono::DateTime::from_timestamp(seconds.floor() as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Seconds per point: fine enough to see a 10-minute episode, coarse enough to stay under ~400 points.
fn period_for(span: f64) -> u32 {
    match span {
        s if s <= 3.0 * 3600.0 => 60,
        s if s <= 26.0 * 3600.0 => 300,
        s if s <= 8.0 * 86_400.0 => 1800,
        _ => 3600,
    }
}

fn reusable(fetch: &Fetch, start: f64, end: f64, now: f64) -> bool {
    let step = f64::from(fetch.signals.period_seconds);
    let (from, to) = (epoch(&fetch.signals.start).unwrap_or(f64::MAX), epoch(&fetch.signals.end).unwrap_or(0.0));
    let fresh = now - fetch.at < FRESH_SECONDS;
    ((to - from) - (end - start)).abs() < 2.0
        && from <= start + step
        && (to >= end - step || fresh)
        && fetch.signals.period_seconds <= period_for(end - start)
}

/// The cloud's view of `info` between `start` and `end` (epoch seconds), from the cache when it is fresh.
pub async fn signals(dir: &Path, info: &ConnectionInfo, start: f64, end: f64, refresh: bool) -> Result<CloudSignals> {
    let target = target(info).ok_or_else(|| {
        Error::new(
            ErrorCode::Unsupported,
            format!("'{}' isn't a cloud database Culprit can read metrics for", info.name),
        )
        .with_hint(
            "Cloud metrics cover Amazon RDS and Aurora (*.rds.amazonaws.com) and Azure SQL Database or Azure Database \
                 for PostgreSQL/MySQL (*.database.windows.net, *.database.azure.com) endpoints",
        )
    })?;
    let now = now();
    let end = end.min(now);
    let start = start.max(end - MAX_DAYS * 86_400.0).min(end - 600.0);
    let cache_key = format!("{}:{}/{}", info.engine, info.host, info.database.as_deref().unwrap_or(""));
    let mut cache = read_cache(dir, &cache_key);
    if !refresh {
        // Reuse a fetch that covers the period at least as finely as a fresh one would.
        if let Some(f) = cache.fetches.iter().find(|f| reusable(f, start, end, now)) {
            // Keep its actual recorded range and assessment together; a different selected span needs a new fetch.
            return Ok(f.signals.clone());
        }
    }
    let resource = match (&cache.resource, cache.resolved_at) {
        (Some(r), Some(at)) if !refresh && now - at < RESOLVE_SECONDS => r.clone(),
        _ => {
            let r = timed(resolve(&target)).await?;
            cache.resource = Some(r.clone());
            cache.resolved_at = Some(now);
            r
        }
    };
    let signals = match &target {
        Target::Rds { .. } => timed(rds_signals(&resource, start, end)).await?,
        Target::AzureSql { .. } | Target::AzureFlexible { .. } => {
            timed(azure_signals(&target, &resource, start, end)).await?
        }
    };
    cache.fetches.retain(|f| f.signals.period_seconds != signals.period_seconds);
    cache.fetches.push(Fetch { signals: signals.clone(), at: now });
    write_cache(dir, &cache);
    Ok(signals)
}

/// "last 24 h", or "6 h up to Tue 14:00 UTC" for a period in the past.
fn span_words(start: f64, end: f64) -> String {
    let length = format::duration((end - start) * 1000.0);
    if now() - end < 900.0 { format!("last {length}") } else { format!("{length} up to {}", short_time(&iso(end))) }
}

async fn timed<T>(work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(CLI_TIMEOUT, work).await.map_err(|_| {
        Error::new(ErrorCode::Timeout, "The cloud CLI didn't answer in time")
            .with_hint("Check the network and the CLI's sign-in, then try again")
    })?
}

// ───────── Amazon RDS ─────────

async fn resolve(target: &Target) -> Result<Resource> {
    match target {
        Target::Rds { region, id, cluster, reader } => {
            let instance = if *cluster { cluster_member(region, id, *reader).await? } else { id.clone() };
            let json = aws_json(&[
                "rds",
                "describe-db-instances",
                "--db-instance-identifier",
                &instance,
                "--region",
                region,
                "--output",
                "json",
            ])
            .await?;
            let i = json
                .get("DBInstances")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .ok_or_else(|| Error::not_found(format!("AWS has no RDS instance '{instance}' in {region}")))?;
            let text = |k: &str| i.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let number = |k: &str| i.get(k).and_then(|v| v.as_f64());
            Ok(Resource {
                service: rds_service(text("Engine").as_deref().unwrap_or_default()),
                name: instance.clone(),
                region: Some(region.clone()),
                instance: Some(instance),
                class: text("DBInstanceClass"),
                storage_type: text("StorageType"),
                allocated_gb: number("AllocatedStorage"),
                iops: number("Iops"),
                insights: i.get("PerformanceInsightsEnabled").and_then(|v| v.as_bool()).unwrap_or(false),
                ..Default::default()
            })
        }
        Target::AzureSql { server, database } => {
            #[derive(Deserialize)]
            struct Row {
                id: String,
                name: String,
                sku: Option<String>,
                tier: Option<String>,
            }
            let query = format!(
                "resources | where type =~ 'microsoft.sql/servers/databases' and name =~ '{database}' \
                 and tostring(split(id, '/')[8]) =~ '{server}' \
                 | project id, name, sku = tostring(sku.name), tier = tostring(sku.tier)"
            );
            let row = discover::resource_graph::<Row>(&query).await?.into_iter().next().ok_or_else(|| {
                Error::not_found(format!("Azure Resource Graph has no database '{database}' on server '{server}'"))
                    .with_hint("Sign in with `az login` to the tenant and subscription that hold it")
            })?;
            Ok(Resource {
                service: "Azure SQL Database".into(),
                name: format!("{server}/{}", row.name),
                azure_id: Some(row.id),
                sku: row.sku.filter(|s| !s.is_empty()),
                sku_tier: row.tier.filter(|s| !s.is_empty()),
                ..Default::default()
            })
        }
        Target::AzureFlexible { engine, server } => {
            #[derive(Deserialize)]
            struct Row {
                id: String,
                name: String,
                sku: Option<String>,
                tier: Option<String>,
                iops: Option<f64>,
                size: Option<f64>,
            }
            let kind = if *engine == Engine::Postgres { "dbforpostgresql" } else { "dbformysql" };
            let query = format!(
                "resources | where type =~ 'microsoft.{kind}/flexibleservers' and name =~ '{server}' \
                 | project id, name, sku = tostring(sku.name), tier = tostring(sku.tier), \
                   iops = todouble(properties.storage.iops), size = todouble(properties.storage.storageSizeGB)"
            );
            let row = discover::resource_graph::<Row>(&query).await?.into_iter().next().ok_or_else(|| {
                Error::not_found(format!("Azure Resource Graph has no flexible server '{server}'"))
                    .with_hint("Sign in with `az login` to the tenant and subscription that hold it")
            })?;
            Ok(Resource {
                service: format!(
                    "Azure Database for {} flexible server",
                    if *engine == Engine::Postgres { "PostgreSQL" } else { "MySQL" }
                ),
                name: row.name,
                azure_id: Some(row.id),
                sku: row.sku.filter(|s| !s.is_empty()),
                sku_tier: row.tier.filter(|s| !s.is_empty()),
                iops: row.iops,
                allocated_gb: row.size,
                ..Default::default()
            })
        }
    }
}

fn rds_service(engine: &str) -> String {
    let product = match engine {
        e if e.starts_with("aurora-postgresql") => return "Amazon Aurora PostgreSQL".into(),
        e if e.starts_with("aurora") => return "Amazon Aurora MySQL".into(),
        e if e.starts_with("postgres") => "PostgreSQL",
        e if e.starts_with("mariadb") => "MariaDB",
        e if e.starts_with("mysql") => "MySQL",
        e if e.starts_with("sqlserver") => "SQL Server",
        _ => return "Amazon RDS".into(),
    };
    format!("Amazon RDS for {product}")
}

async fn cluster_member(region: &str, cluster: &str, reader: bool) -> Result<String> {
    let json = aws_json(&[
        "rds",
        "describe-db-clusters",
        "--db-cluster-identifier",
        cluster,
        "--region",
        region,
        "--output",
        "json",
    ])
    .await?;
    let members = json
        .get("DBClusters")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("DBClusterMembers"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    members
        .iter()
        .filter(|m| m.get("IsClusterWriter").and_then(|w| w.as_bool()) == Some(!reader))
        .chain(members.iter())
        .find_map(|m| m.get("DBInstanceIdentifier").and_then(|i| i.as_str()).map(str::to_string))
        .ok_or_else(|| Error::not_found(format!("Aurora cluster '{cluster}' has no instances in {region}")))
}

async fn aws_json(args: &[&str]) -> Result<Value> {
    let bytes = aws::run(args).await.map_err(|e| match e.code {
        ErrorCode::PermissionDenied => e.with_hint(
            "The signed-in AWS identity needs rds:DescribeDBInstances, rds:DescribeDBClusters and cloudwatch:GetMetricData",
        ),
        _ => e,
    })?;
    serde_json::from_slice(&bytes).map_err(|e| Error::internal(format!("Unexpected AWS CLI output: {e}")))
}

/// (query id, CloudWatch metric, statistic)
const RDS_METRICS: &[(&str, &str, &str)] = &[
    ("cpu", "CPUUtilization", "Average"),
    ("credits", "CPUCreditBalance", "Minimum"),
    ("surplus", "CPUSurplusCreditBalance", "Maximum"),
    ("burst", "BurstBalance", "Minimum"),
    ("riops", "ReadIOPS", "Average"),
    ("wiops", "WriteIOPS", "Average"),
    ("rlat", "ReadLatency", "Average"),
    ("wlat", "WriteLatency", "Average"),
    ("load", "DBLoad", "Average"),
    ("mem", "FreeableMemory", "Minimum"),
    ("conns", "DatabaseConnections", "Maximum"),
    ("ebsio", "EBSIOBalance%", "Minimum"),
    ("ebsbytes", "EBSByteBalance%", "Minimum"),
    ("storage", "FreeStorageSpace", "Minimum"),
];

async fn rds_signals(resource: &Resource, start: f64, end: f64) -> Result<CloudSignals> {
    let (Some(region), Some(instance)) = (&resource.region, &resource.instance) else {
        return Err(Error::internal("RDS resource without a region or instance"));
    };
    let period = period_for(end - start);
    let queries: Vec<Value> = RDS_METRICS
        .iter()
        .map(|(id, metric, stat)| {
            serde_json::json!({
                "Id": id,
                "MetricStat": {
                    "Metric": {
                        "Namespace": "AWS/RDS",
                        "MetricName": metric,
                        "Dimensions": [{ "Name": "DBInstanceIdentifier", "Value": instance }]
                    },
                    "Period": period,
                    "Stat": stat
                },
                "ReturnData": true
            })
        })
        .collect();
    let queries = Value::Array(queries).to_string();
    let (from, to) = (iso(start), iso(end));
    let json = aws_json(&[
        "cloudwatch",
        "get-metric-data",
        "--region",
        region,
        "--start-time",
        &from,
        "--end-time",
        &to,
        "--scan-by",
        "TimestampAscending",
        "--metric-data-queries",
        &queries,
        "--output",
        "json",
    ])
    .await?;
    let mut raw: std::collections::BTreeMap<String, Vec<CloudPoint>> = Default::default();
    for result in json.get("MetricDataResults").and_then(|r| r.as_array()).into_iter().flatten() {
        let id = result.get("Id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
        let times = result.get("Timestamps").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        let values = result.get("Values").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut points: Vec<CloudPoint> = times
            .iter()
            .zip(values.iter())
            .filter_map(|(t, v)| Some(CloudPoint { at: normalize_time(t.as_str()?)?, value: v.as_f64()? }))
            .collect();
        points.sort_by(|a, b| a.at.cmp(&b.at));
        raw.insert(id, points);
    }
    Ok(rds_judge(resource, raw, start, end, period))
}

/// CloudWatch and Azure report `2026-10-11T12:05:00+00:00` or `…Z`; keep one form so series line up.
fn normalize_time(text: &str) -> Option<String> {
    epoch(text).map(iso)
}

fn rds_judge(
    resource: &Resource,
    mut raw: std::collections::BTreeMap<String, Vec<CloudPoint>>,
    start: f64,
    end: f64,
    period: u32,
) -> CloudSignals {
    let mut take = |id: &str| raw.remove(id).unwrap_or_default();
    let class = resource.class.clone().unwrap_or_default();
    let vcpus = rds_vcpus(&class);
    let baseline = rds_cpu_baseline(&class);
    let storage = resource.storage_type.clone().unwrap_or_default();
    let gb = resource.allocated_gb.unwrap_or(0.0);
    let mut series = Vec::new();
    let mut found: Vec<ThrottleEpisode> = Vec::new();

    let cpu = CloudSeries::new("CPUUtilization", "CPU", CloudUnit::Percent, take("cpu"));
    let cpu = match baseline {
        Some(b) => cpu.limit(b, format!("baseline {b:.0}%")),
        None => cpu,
    };
    let saturated = |worst: f64, lasted: &str| {
        format!(
            "CPU was above 90% in the recorded samples over {lasted}, peaking at {worst:.0}%. That leaves little headroom; these metrics alone do not measure query delays."
        )
    };
    found.extend(episodes(
        &cpu,
        period,
        &rule("CPUUtilization", ThrottleKind::CpuLimit, 90.0, false, 3, "CPU use was high", &saturated),
    ));
    series.push(cpu);

    let credits = take("credits");
    let surplus = take("surplus");
    if baseline.is_some() && !credits.is_empty() {
        let unlimited = surplus.iter().any(|p| p.value > 0.0);
        let b = baseline.unwrap_or_default();
        let detail = move |_: f64, lasted: &str| {
            if unlimited {
                format!(
                    "The credit balance was zero in samples over {lasted}, and surplus credits were used. Unlimited mode can continue above the {b:.0}% baseline; these counters alone do not establish query delays."
                )
            } else {
                format!(
                    "The credit balance was zero in samples over {lasted}. In standard mode, demand above its {b:.0}% CPU baseline is limited; unlimited mode can keep bursting. Credit balance alone does not establish longer query times."
                )
            }
        };
        let s = CloudSeries::new("CPUCreditBalance", "CPU credits", CloudUnit::Credits, credits).floor();
        found.extend(episodes(
            &s,
            period,
            &rule("CPUCreditBalance", ThrottleKind::CpuCredits, 0.0, true, 2, "CPU credits ran out", &detail),
        ));
        series.push(s);
    }

    let (read, write) = (take("riops"), take("wiops"));
    let read = CloudSeries::new("ReadIOPS", "Read IOPS", CloudUnit::Iops, read);
    let write = CloudSeries::new("WriteIOPS", "Write IOPS", CloudUnit::Iops, write);
    if let Some(iops) = sum_series("IOPS", "IOPS", CloudUnit::Iops, &[&read, &write]) {
        let provisioned = resource.iops.filter(|i| *i > 0.0 && matches!(storage.as_str(), "gp3" | "io1" | "io2"));
        let iops = match (provisioned, storage.as_str()) {
            (Some(limit), _) => {
                let limit_words = format!("provisioned {} IOPS", format::count(limit));
                let detail = move |worst: f64, lasted: &str| {
                    format!(
                        "Reads and writes reached {} IOPS against the provisioned {} in samples over {lasted}. Near a provisioned limit leaves little headroom; confirm read and write latency before changing capacity.",
                        format::count(worst),
                        format::count(limit)
                    )
                };
                let s = iops.limit(limit, limit_words);
                found.extend(episodes(
                    &s,
                    period,
                    &rule(
                        "IOPS",
                        ThrottleKind::IoLimit,
                        limit * 0.95,
                        false,
                        3,
                        "IOPS were near the provisioned limit",
                        &detail,
                    ),
                ));
                s
            }
            (None, "gp2") if gb > 0.0 => {
                let base = gp2_baseline_iops(gb);
                iops.limit(base, format!("baseline {} IOPS (3 per GB)", format::count(base)))
            }
            _ => iops,
        };
        series.push(iops);
    }

    let burst = take("burst");
    if !burst.is_empty() {
        let base = gp2_baseline_iops(gb);
        let detail = move |_: f64, lasted: &str| {
            format!(
                "The volume's burst balance was depleted in samples over {lasted}; its baseline is {} IOPS. This alone does not establish query delays.",
                format::count(base)
            )
        };
        let s = CloudSeries::new("BurstBalance", "Storage burst balance", CloudUnit::Percent, burst).floor();
        found.extend(episodes(
            &s,
            period,
            &rule("BurstBalance", ThrottleKind::IoCredits, 0.0, true, 2, "Storage burst credits ran out", &detail),
        ));
        series.push(s);
    }

    for (id, metric, label) in
        [("ebsio", "EBSIOBalance%", "EBS IO balance"), ("ebsbytes", "EBSByteBalance%", "EBS throughput balance")]
    {
        let points = take(id);
        if points.is_empty() {
            continue;
        }
        let class_words = class.clone();
        let detail = move |_: f64, lasted: &str| {
            format!(
                "{class_words}'s EBS burst balance was depleted in samples over {lasted}. Demand above the baseline can be limited; compare actual I/O demand and latency before changing capacity."
            )
        };
        let s = CloudSeries::new(metric, label, CloudUnit::Percent, points).floor();
        found.extend(episodes(
            &s,
            period,
            &rule(metric, ThrottleKind::IoCredits, 0.0, true, 2, "EBS burst ran out", &detail),
        ));
        series.push(s);
    }

    let read_lat = CloudSeries::new("ReadLatency", "Read latency", CloudUnit::Ms, scale(take("rlat"), 1000.0));
    let write_lat = CloudSeries::new("WriteLatency", "Write latency", CloudUnit::Ms, scale(take("wlat"), 1000.0));
    if let Some(latency) = max_series("DiskLatency", "Disk latency", CloudUnit::Ms, &read_lat, &write_lat) {
        let detail = |worst: f64, lasted: &str| {
            format!("Reads or writes took {} on average at worst, above 20 ms for {lasted}.", format::duration(worst))
        };
        found.extend(episodes(
            &latency,
            period,
            &rule("DiskLatency", ThrottleKind::DiskLatency, 20.0, false, 3, "Storage answered slowly", &detail),
        ));
        series.push(latency);
    }

    let load = take("load");
    if !load.is_empty() {
        let s = CloudSeries::new("DBLoad", "DB load", CloudUnit::Sessions, load);
        let s = match vcpus {
            Some(v) => {
                let detail = move |worst: f64, lasted: &str| {
                    format!(
                        "The recorded active-session count peaked at {worst:.1} on {v:.0} vCPUs over {lasted}. Active sessions can be running or waiting on many kinds of events; this count alone does not establish a CPU shortage."
                    )
                };
                let s = s.limit(v, format!("{v:.0} vCPUs"));
                found.extend(episodes(
                    &s,
                    period,
                    &rule("DBLoad", ThrottleKind::Load, v * 1.5, false, 3, "More work than vCPUs", &detail),
                ));
                s
            }
            None => s,
        };
        series.push(s);
    }

    let free = take("storage");
    if !free.is_empty() && gb > 0.0 {
        let floor = gb * 1e9 * 0.05;
        let detail = |worst: f64, lasted: &str| {
            format!("Free storage fell to {} for {lasted}; writes fail when it runs out.", format::bytes(worst))
        };
        let s = CloudSeries::new("FreeStorageSpace", "Free storage", CloudUnit::Bytes, free)
            .floor()
            .limit(floor, format!("alert below {} (5% of {gb:.0} GB)", format::bytes(floor)));
        found.extend(episodes(
            &s,
            period,
            &rule("FreeStorageSpace", ThrottleKind::Storage, floor, true, 2, "Storage nearly full", &detail),
        ));
        series.push(s);
    }
    let mem = take("mem");
    if !mem.is_empty() {
        series.push(CloudSeries::new("FreeableMemory", "Freeable memory", CloudUnit::Bytes, mem).floor());
    }
    let conns = take("conns");
    if !conns.is_empty() {
        series.push(CloudSeries::new("DatabaseConnections", "Connections", CloudUnit::Sessions, conns));
    }

    let mut tier = vec![];
    if !class.is_empty() {
        let mut words = class.clone();
        let mut extra = vec![];
        if baseline.is_some() {
            extra.push("burstable".to_string());
        }
        if let Some(v) = vcpus {
            extra.push(format!("{v:.0} vCPUs"));
        }
        if !extra.is_empty() {
            words = format!("{words} ({})", extra.join(", "));
        }
        tier.push(words);
    }
    if !storage.is_empty() {
        let mut words = storage.clone();
        if gb > 0.0 {
            words = format!("{words}, {gb:.0} GB");
        }
        if let Some(iops) = resource.iops.filter(|i| *i > 0.0) {
            words = format!("{words}, {} IOPS", format::count(iops));
        }
        tier.push(words);
    }
    let mut notes = Vec::new();
    if !resource.insights {
        notes.push(
            "Performance Insights is off, so DB load isn't measured; turning it on adds the DBLoad metric.".to_string(),
        );
    }
    finish(CloudProvider::Aws, resource, tier, series, found, notes, start, end, period, "CloudWatch")
}

fn rule<'a>(
    metric: &'a str,
    kind: ThrottleKind,
    threshold: f64,
    floor: bool,
    min_points: usize,
    title: &'a str,
    detail: &'a dyn Fn(f64, &str) -> String,
) -> Rule<'a> {
    Rule { metric, kind, threshold, floor, min_points, title, detail }
}

fn scale(points: Vec<CloudPoint>, factor: f64) -> Vec<CloudPoint> {
    points.into_iter().map(|p| CloudPoint { value: p.value * factor, ..p }).collect()
}

/// The higher of two series at each time (read or write latency, whichever is worse).
fn max_series(metric: &str, label: &str, unit: CloudUnit, a: &CloudSeries, b: &CloudSeries) -> Option<CloudSeries> {
    let mut points: Vec<CloudPoint> = a.points.clone();
    for p in &b.points {
        match points.iter_mut().find(|x| x.at == p.at) {
            Some(x) => x.value = x.value.max(p.value),
            None => points.push(p.clone()),
        }
    }
    points.sort_by(|x, y| x.at.cmp(&y.at));
    (!points.is_empty()).then(|| CloudSeries::new(metric, label, unit, points))
}

#[allow(clippy::too_many_arguments)]
fn finish(
    provider: CloudProvider,
    resource: &Resource,
    tier: Vec<String>,
    series: Vec<CloudSeries>,
    mut found: Vec<ThrottleEpisode>,
    mut notes: Vec<String>,
    start: f64,
    end: f64,
    period: u32,
    source: &str,
) -> CloudSignals {
    found.sort_by(|a, b| a.start.cmp(&b.start));
    let span = span_words(start, end);
    let no_data = series.iter().all(|s| s.points.is_empty());
    if no_data {
        notes.push(format!("{source} returned no data points for this period."));
    }
    let minutes = format::duration(f64::from(period) * 1000.0);
    notes.push(format!("From {source}, one point per {minutes}."));
    CloudSignals {
        provider,
        service: resource.service.clone(),
        resource: resource.name.clone(),
        tier: (!tier.is_empty()).then(|| tier.join(" · ")),
        start: iso(start),
        end: iso(end),
        period_seconds: period,
        summary: if no_data {
            format!("No cloud measurements were available for the {span}.")
        } else {
            summarize(&found, &span)
        },
        series,
        episodes: found,
        notes,
        fetched_at: iso(now()),
    }
}

// ───────── Azure ─────────

fn az_interval(period: u32) -> &'static str {
    match period {
        60 => "PT1M",
        300 => "PT5M",
        1800 => "PT30M",
        _ => "PT1H",
    }
}

/// (metric, label, unit) per resource kind.
fn azure_metrics(target: &Target) -> &'static [(&'static str, &'static str, CloudUnit)] {
    match target {
        Target::AzureSql { .. } => &[
            ("cpu_percent", "CPU", CloudUnit::Percent),
            ("physical_data_read_percent", "Data IO", CloudUnit::Percent),
            ("log_write_percent", "Log writes", CloudUnit::Percent),
            ("workers_percent", "Workers", CloudUnit::Percent),
            ("sessions_percent", "Sessions", CloudUnit::Percent),
        ],
        Target::AzureFlexible { engine: Engine::Postgres, .. } => &[
            ("cpu_percent", "CPU", CloudUnit::Percent),
            ("memory_percent", "Memory", CloudUnit::Percent),
            ("disk_iops_consumed_percentage", "IOPS used", CloudUnit::Percent),
            ("cpu_credits_remaining", "CPU credits", CloudUnit::Credits),
            ("storage_percent", "Storage used", CloudUnit::Percent),
            ("active_connections", "Connections", CloudUnit::Sessions),
        ],
        _ => &[
            ("cpu_percent", "CPU", CloudUnit::Percent),
            ("memory_percent", "Memory", CloudUnit::Percent),
            ("io_consumption_percent", "IO used", CloudUnit::Percent),
            ("cpu_credits_remaining", "CPU credits", CloudUnit::Credits),
            ("storage_percent", "Storage used", CloudUnit::Percent),
            ("active_connections", "Connections", CloudUnit::Sessions),
        ],
    }
}

async fn az_metrics(id: &str, metrics: &[&str], start: f64, end: f64, period: u32) -> Result<Value> {
    let (from, to) = (iso(start), iso(end));
    let mut args: Vec<&str> = vec!["monitor", "metrics", "list", "--resource", id, "--metrics"];
    args.extend(metrics);
    args.extend([
        "--start-time",
        &from,
        "--end-time",
        &to,
        "--interval",
        az_interval(period),
        "--aggregation",
        "Average",
        "--output",
        "json",
    ]);
    let output = crate::cloud_cli::command(crate::cloud_cli::Cli::Azure).args(&args).output().await.map_err(|e| {
        Error::new(ErrorCode::AuthFailed, format!("Could not run the Azure CLI: {e}"))
            .with_hint("Install the Azure CLI and run `az login`")
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = if stderr.contains("az login") || stderr.contains("AADSTS") {
            ErrorCode::AuthFailed
        } else if stderr.contains("AuthorizationFailed") {
            ErrorCode::PermissionDenied
        } else {
            ErrorCode::QueryFailed
        };
        return Err(Error::new(code, format!("Azure Monitor query failed: {stderr}")).with_hint(
            "Run `az login`; reading metrics needs the Monitoring Reader role (or Reader) on the resource",
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| Error::internal(format!("Unexpected Azure CLI output: {e}")))
}

async fn azure_signals(target: &Target, resource: &Resource, start: f64, end: f64) -> Result<CloudSignals> {
    let id = resource.azure_id.clone().ok_or_else(|| Error::internal("Azure resource without an id"))?;
    let period = period_for(end - start);
    let wanted = azure_metrics(target);
    let names: Vec<&str> = wanted.iter().map(|(m, _, _)| *m).collect();
    // A metric this resource doesn't publish fails the whole call; fall back to the ones every tier has.
    let mut notes = Vec::new();
    let json = match az_metrics(&id, &names, start, end, period).await {
        Ok(json) => json,
        Err(e) if e.code == ErrorCode::QueryFailed => {
            notes.push("Some metrics aren't published for this tier; showing CPU, memory and storage.".to_string());
            let basic: Vec<&str> = names
                .iter()
                .copied()
                .filter(|m| matches!(*m, "cpu_percent" | "memory_percent" | "storage_percent"))
                .collect();
            az_metrics(&id, &basic, start, end, period).await?
        }
        Err(e) => return Err(e),
    };
    let mut raw: std::collections::BTreeMap<String, Vec<CloudPoint>> = Default::default();
    for metric in json.get("value").and_then(|v| v.as_array()).into_iter().flatten() {
        let name = metric.get("name").and_then(|n| n.get("value")).and_then(|v| v.as_str()).unwrap_or_default();
        let mut points: Vec<CloudPoint> = metric
            .get("timeseries")
            .and_then(|t| t.as_array())
            .into_iter()
            .flatten()
            .flat_map(|ts| ts.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default())
            .filter_map(|d| {
                Some(CloudPoint {
                    at: normalize_time(d.get("timeStamp")?.as_str()?)?,
                    value: d.get("average")?.as_f64()?,
                })
            })
            .collect();
        points.sort_by(|a, b| a.at.cmp(&b.at));
        raw.insert(name.to_string(), points);
    }
    Ok(azure_judge(target, resource, raw, notes, start, end, period))
}

/// How an Azure metric is judged: the kind of episode, the threshold, whether lower is worse, the title and the sentence.
type Judged = (ThrottleKind, f64, bool, &'static str, Box<dyn Fn(f64, &str) -> String>);

fn azure_judge(
    target: &Target,
    resource: &Resource,
    mut raw: std::collections::BTreeMap<String, Vec<CloudPoint>>,
    notes: Vec<String>,
    start: f64,
    end: f64,
    period: u32,
) -> CloudSignals {
    let sku = resource.sku.clone().unwrap_or_else(|| "this tier".to_string());
    let burstable = resource.sku_tier.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("burstable"));
    let mut series = Vec::new();
    let mut found = Vec::new();
    for (metric, label, unit) in azure_metrics(target) {
        let points = raw.remove(*metric).unwrap_or_default();
        if points.is_empty() {
            continue;
        }
        let mut s = CloudSeries::new(metric, label, *unit, points);
        let sku_words = sku.clone();
        let limit_detail = move |what: &'static str, waits: &'static str| {
            let sku_words = sku_words.clone();
            move |worst: f64, lasted: &str| {
                format!(
                    "{what} reached {worst:.0}% of {sku_words}'s limit in recorded samples over {lasted}. Near a limit leaves little headroom; actual throttling or query delay is not measured by this percentage{waits}."
                )
            }
        };
        let judged: Option<Judged> = match (*metric, target) {
            ("cpu_percent", Target::AzureSql { .. }) => Some((
                ThrottleKind::CpuLimit,
                95.0,
                false,
                "CPU was near the tier's limit",
                Box::new(limit_detail("CPU", "")),
            )),
            ("cpu_percent", _) => {
                Some((ThrottleKind::CpuLimit, 95.0, false, "CPU use was high", Box::new(limit_detail("CPU", ""))))
            }
            ("physical_data_read_percent", _) => Some((
                ThrottleKind::IoLimit,
                95.0,
                false,
                "Data IO was near the tier's limit",
                Box::new(limit_detail("Data IO", "")),
            )),
            ("log_write_percent", _) => Some((
                ThrottleKind::LogRate,
                95.0,
                false,
                "Log writes were near the tier's limit",
                Box::new(limit_detail("Log writes", "")),
            )),
            ("workers_percent", _) => Some((
                ThrottleKind::Workers,
                90.0,
                false,
                "Worker use neared the tier's limit",
                Box::new(limit_detail("Workers in use", "; requests can be refused at the cap")),
            )),
            ("sessions_percent", _) => Some((
                ThrottleKind::Workers,
                90.0,
                false,
                "Sessions neared the tier's limit",
                Box::new(limit_detail("Sessions", "; connections can be refused at 100%")),
            )),
            ("disk_iops_consumed_percentage" | "io_consumption_percent", _) => Some((
                ThrottleKind::IoLimit,
                95.0,
                false,
                "Storage IOPS were near their limit",
                Box::new(limit_detail("IOPS", "")),
            )),
            // Allocated memory can include useful cache; a high percentage alone establishes no shortage.
            ("memory_percent", _) => None,
            ("storage_percent", _) => Some((
                ThrottleKind::Storage,
                95.0,
                false,
                "Storage nearly full",
                Box::new(limit_detail("Storage used", "")),
            )),
            ("cpu_credits_remaining", _) if burstable => Some((
                ThrottleKind::CpuCredits,
                0.0,
                true,
                "CPU credits ran out",
                Box::new(|_: f64, lasted: &str| {
                    format!(
                        "The Burstable server's CPU-credit balance was zero in samples over {lasted}. Demand above baseline can be limited; these counters alone do not measure query delays."
                    )
                }),
            )),
            _ => None,
        };
        if matches!(unit, CloudUnit::Percent) {
            s = s.limit(100.0, "tier limit");
        }
        if *metric == "cpu_credits_remaining" {
            s = s.floor();
            s.limit = None;
            s.limit_label = None;
        }
        if let Some((kind, threshold, floor, title, detail)) = judged {
            found.extend(episodes(
                &s,
                period,
                &Rule { metric, kind, threshold, floor, min_points: 2, title, detail: &*detail },
            ));
        }
        series.push(s);
    }
    let tier = [resource.sku.clone(), resource.sku_tier.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let tier = if let Some(iops) = resource.iops.filter(|i| *i > 0.0) {
        vec![tier, format!("{} IOPS", format::count(iops))]
    } else {
        vec![tier]
    };
    let tier: Vec<String> = tier.into_iter().filter(|t| !t.is_empty()).collect();
    finish(CloudProvider::Azure, resource, tier, series, found, notes, start, end, period, "Azure Monitor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{AuthKind, Environment};

    fn info(engine: Engine, host: &str, database: Option<&str>) -> ConnectionInfo {
        ConnectionInfo {
            name: "x".into(),
            engine,
            environment: Environment::Production,
            host: host.into(),
            port: None,
            database: database.map(str::to_string),
            user: None,
            auth: AuthKind::Password,
            has_secret: true,
            display: String::new(),
            last_report: None,
            agent_access: Default::default(),
            query_log: None,
        }
    }

    #[test]
    fn a_cached_day_cannot_supply_an_hours_assessment() {
        let start = 1_791_720_000.0;
        let end = start + 86400.0;
        let signals = rds_judge(&Resource::default(), Default::default(), start, end, 300);
        let fetch = Fetch { signals, at: end };
        assert!(reusable(&fetch, start, end, end));
        assert!(!reusable(&fetch, end - 3600.0, end, end));
        assert!(!reusable(&fetch, end - 6.0 * 3600.0, end, end));
        assert!(!reusable(&fetch, start, end + 86400.0, end + 86400.0));
    }

    #[test]
    fn hosts_map_to_cloud_resources() {
        assert_eq!(
            target(&info(Engine::Postgres, "shop-db.abc123.us-east-1.rds.amazonaws.com", None)),
            Some(Target::Rds { region: "us-east-1".into(), id: "shop-db".into(), cluster: false, reader: false })
        );
        assert_eq!(
            target(&info(Engine::Mysql, "shop.cluster-ro-abc.eu-central-1.rds.amazonaws.com", None)),
            Some(Target::Rds { region: "eu-central-1".into(), id: "shop".into(), cluster: true, reader: true })
        );
        assert_eq!(target(&info(Engine::Postgres, "p.proxy-abc.us-east-1.rds.amazonaws.com", None)), None);
        assert_eq!(
            target(&info(Engine::Mssql, "contoso.database.windows.net", Some("orders"))),
            Some(Target::AzureSql { server: "contoso".into(), database: "orders".into() })
        );
        assert_eq!(target(&info(Engine::Mssql, "contoso.database.windows.net", Some("o'rders"))), None);
        assert_eq!(
            target(&info(Engine::Postgres, "pg1.postgres.database.azure.com", Some("app"))),
            Some(Target::AzureFlexible { engine: Engine::Postgres, server: "pg1".into() })
        );
        assert_eq!(target(&info(Engine::Postgres, "localhost", None)), None);
        assert_eq!(target(&info(Engine::Postgres, "db.example.com", None)), None);
    }

    fn points(values: &[f64]) -> Vec<CloudPoint> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| CloudPoint { at: iso(1_791_720_000.0 + i as f64 * 300.0), value: *v })
            .collect()
    }

    #[test]
    fn zero_credit_balance_warns_without_asserting_query_throttling() {
        let resource = Resource {
            service: "Amazon RDS for PostgreSQL".into(),
            name: "shop".into(),
            class: Some("db.t3.small".into()),
            storage_type: Some("gp2".into()),
            allocated_gb: Some(20.0),
            ..Default::default()
        };
        let mut raw = std::collections::BTreeMap::new();
        raw.insert("cpu".to_string(), points(&[30.0, 25.0, 20.0, 20.0, 20.0, 20.0]));
        raw.insert("credits".to_string(), points(&[12.0, 3.0, 0.0, 0.0, 0.0, 0.2]));
        raw.insert("riops".to_string(), points(&[50.0, 60.0, 70.0, 80.0, 90.0, 100.0]));
        raw.insert("wiops".to_string(), points(&[5.0, 5.0, 5.0, 5.0, 5.0, 5.0]));
        let s = rds_judge(&resource, raw, 1_791_720_000.0, 1_791_721_800.0, 300);
        assert_eq!(s.episodes.len(), 1, "{:?}", s.episodes);
        let e = &s.episodes[0];
        assert_eq!(e.kind, ThrottleKind::CpuCredits);
        assert!(!e.ongoing, "a positive credit balance has recovered");
        assert!(e.detail.contains("20% CPU baseline") && e.detail.contains("alone does not establish"), "{}", e.detail);
        assert_eq!(s.tier.as_deref(), Some("db.t3.small (burstable, 2 vCPUs) · gp2, 20 GB"));
        let iops = s.series("IOPS").unwrap();
        assert_eq!(iops.limit, Some(100.0));
        assert_eq!(iops.points[5].value, 105.0);
        assert!(s.summary.contains("CPU credits ran out once"), "{}", s.summary);
        assert!(s.notes.iter().any(|n| n.contains("Performance Insights is off")));
    }

    #[test]
    fn azure_sql_at_its_log_rate_limit() {
        let resource = Resource {
            service: "Azure SQL Database".into(),
            name: "contoso/orders".into(),
            sku: Some("GP_Gen5_2".into()),
            sku_tier: Some("GeneralPurpose".into()),
            ..Default::default()
        };
        let mut raw = std::collections::BTreeMap::new();
        raw.insert("cpu_percent".to_string(), points(&[40.0, 45.0, 50.0]));
        raw.insert("log_write_percent".to_string(), points(&[60.0, 99.0, 100.0]));
        let target = Target::AzureSql { server: "contoso".into(), database: "orders".into() };
        let s = azure_judge(&target, &resource, raw, vec![], 1_791_720_000.0, 1_791_720_900.0, 300);
        assert_eq!(s.episodes.len(), 1);
        assert_eq!(s.episodes[0].kind, ThrottleKind::LogRate);
        assert_eq!(s.episodes[0].status, culprit_core::SignalStatus::Watch);
        assert!(s.episodes[0].detail.contains("not measured by this percentage"), "{}", s.episodes[0].detail);
        assert_eq!(s.series("cpu_percent").unwrap().limit, Some(100.0));
    }

    #[test]
    fn azure_allocated_memory_is_visible_without_asserting_a_shortage() {
        let target = Target::AzureFlexible { engine: Engine::Postgres, server: "shop".into() };
        let mut raw = std::collections::BTreeMap::new();
        raw.insert("memory_percent".to_string(), points(&[100.0, 100.0, 100.0]));
        let s = azure_judge(&target, &Resource::default(), raw, vec![], 1_791_720_000.0, 1_791_720_900.0, 300);
        assert!(s.series("memory_percent").is_some());
        assert!(s.episodes.is_empty(), "a full cache does not establish memory pressure");
    }
}
