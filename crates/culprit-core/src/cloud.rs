//! Cloud limits: what the cloud measured for a database server (CloudWatch, Azure Monitor) and the moments it held the
//! server back: CPU credits or storage burst run out, a tier's CPU, IO, log-rate or worker limit hit. A query that got
//! slower with the same plan is often just that, and the database's own statistics can't see it.
//!
//! Fetching lives in culprit-engine (through the person's own `aws` / `az` sign-in, read-only); this module judges the
//! numbers, so the rules are unit-tested without a cloud account.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::changes::epoch;
use crate::events::{ChangeEvent, EventKind, EventSource};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum CloudProvider {
    Aws,
    Azure,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct CloudPoint {
    /// Start of the period (UTC, ISO 8601).
    pub at: String,
    pub value: f64,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum CloudUnit {
    Percent,
    Credits,
    Iops,
    Ms,
    Bytes,
    Sessions,
}

/// One metric over the period, with the limit it is judged against when there is one.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct CloudSeries {
    /// The provider's metric name (`CPUUtilization`, `cpu_percent`).
    pub metric: String,
    /// "CPU", "CPU credits", "IOPS".
    pub label: String,
    pub unit: CloudUnit,
    pub points: Vec<CloudPoint>,
    pub limit: Option<f64>,
    /// "baseline 20%", "provisioned 3,000 IOPS", "2 vCPUs".
    pub limit_label: Option<String>,
    /// Lower is worse (credit balances, free memory): the limit is a floor.
    pub floor: bool,
}

impl CloudSeries {
    pub fn new(metric: &str, label: &str, unit: CloudUnit, points: Vec<CloudPoint>) -> CloudSeries {
        CloudSeries {
            metric: metric.to_string(),
            label: label.to_string(),
            unit,
            points,
            limit: None,
            limit_label: None,
            floor: false,
        }
    }

    pub fn limit(mut self, limit: f64, label: impl Into<String>) -> CloudSeries {
        self.limit = Some(limit);
        self.limit_label = Some(label.into());
        self
    }

    pub fn floor(mut self) -> CloudSeries {
        self.floor = true;
        self
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ThrottleKind {
    /// Burstable instance out of CPU credits: held to its baseline (or billed for surplus credits).
    CpuCredits,
    /// CPU at the instance's or tier's limit.
    CpuLimit,
    /// Storage or instance IO burst credits ran out.
    IoCredits,
    /// IOPS or data IO at the provisioned or tier limit.
    IoLimit,
    /// Transaction log writes at the tier's rate limit.
    LogRate,
    /// Worker threads or sessions at the tier's limit.
    Workers,
    Memory,
    Storage,
    /// Storage answered slowly.
    DiskLatency,
    /// More sessions wanted CPU or IO than the instance has vCPUs.
    Load,
}

/// A stretch of time the cloud held the server back.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ThrottleEpisode {
    pub kind: ThrottleKind,
    pub start: String,
    pub end: String,
    pub metric: String,
    /// The worst value in the episode (lowest for floors).
    pub worst: f64,
    /// "CPU credits ran out".
    pub title: String,
    /// What it meant for queries, in a sentence or two.
    pub detail: String,
    /// Still going at the end of the period.
    pub ongoing: bool,
}

/// What the cloud measured for a server over a period, and when it held the server back.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct CloudSignals {
    pub provider: CloudProvider,
    /// "Amazon RDS for PostgreSQL", "Azure SQL Database".
    pub service: String,
    /// The instance or server as the cloud names it.
    pub resource: String,
    /// Size and storage in words: "db.t3.medium (burstable, 2 vCPUs) · gp2, 100 GB".
    pub tier: Option<String>,
    pub start: String,
    pub end: String,
    pub period_seconds: u32,
    pub series: Vec<CloudSeries>,
    pub episodes: Vec<ThrottleEpisode>,
    /// One or two sentences: what held the server back, or that nothing did.
    pub summary: String,
    pub notes: Vec<String>,
    pub fetched_at: String,
}

impl CloudSignals {
    /// The episodes as timeline events, so they line up with deploys, schema changes and query regressions.
    pub fn events(&self) -> Vec<ChangeEvent> {
        self.episodes
            .iter()
            .map(|e| {
                ChangeEvent::new(EventKind::Throttle, EventSource::Cloud, e.kind_slug(), &e.start, e.title.clone())
                    .until(&e.end)
                    .detail(e.detail.clone())
                    .object(self.resource.clone())
            })
            .collect()
    }

    pub fn series(&self, metric: &str) -> Option<&CloudSeries> {
        self.series.iter().find(|s| s.metric == metric)
    }
}

impl ThrottleEpisode {
    fn kind_slug(&self) -> &'static str {
        match self.kind {
            ThrottleKind::CpuCredits => "cpu-credits",
            ThrottleKind::CpuLimit => "cpu-limit",
            ThrottleKind::IoCredits => "io-credits",
            ThrottleKind::IoLimit => "io-limit",
            ThrottleKind::LogRate => "log-rate",
            ThrottleKind::Workers => "workers",
            ThrottleKind::Memory => "memory",
            ThrottleKind::Storage => "storage",
            ThrottleKind::DiskLatency => "disk-latency",
            ThrottleKind::Load => "load",
        }
    }
}

/// How one series is judged.
pub struct Rule<'a> {
    pub metric: &'a str,
    pub kind: ThrottleKind,
    /// A point is bad when it crosses this: above it, or at or below it for floors.
    pub threshold: f64,
    pub floor: bool,
    /// Consecutive bad points before it counts (one 5-minute spike is not an episode).
    pub min_points: usize,
    pub title: &'a str,
    /// The sentence for an episode, given its worst value and how long it lasted.
    pub detail: &'a dyn Fn(f64, &str) -> String,
}

/// Episodes in `series` under `rule`: runs of bad points at least `min_points` long; a single good point between two
/// runs doesn't split them.
pub fn episodes(series: &CloudSeries, period_seconds: u32, rule: &Rule) -> Vec<ThrottleEpisode> {
    let bad = |v: f64| if rule.floor { v <= rule.threshold } else { v >= rule.threshold };
    let points = &series.points;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, p) in points.iter().enumerate() {
        if !bad(p.value) {
            continue;
        }
        match runs.last_mut() {
            Some((_, end)) if i <= *end + 2 => *end = i,
            _ => runs.push((i, i)),
        }
    }
    let step = f64::from(period_seconds.max(1));
    runs.into_iter()
        .filter(|(a, b)| points[*a..=*b].iter().filter(|p| bad(p.value)).count() >= rule.min_points.max(1))
        .map(|(a, b)| {
            let worst = points[a..=b].iter().map(|p| p.value).fold(
                if rule.floor { f64::INFINITY } else { f64::NEG_INFINITY },
                |w, v| {
                    if rule.floor { w.min(v) } else { w.max(v) }
                },
            );
            let start = points[a].at.clone();
            let end_epoch = epoch(&points[b].at).unwrap_or(0.0) + step;
            let end = chrono::DateTime::from_timestamp(end_epoch as i64, 0)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_else(|| points[b].at.clone());
            let lasted = crate::format::duration((end_epoch - epoch(&start).unwrap_or(end_epoch)).max(step) * 1000.0);
            ThrottleEpisode {
                kind: rule.kind,
                metric: series.metric.clone(),
                worst,
                title: rule.title.to_string(),
                detail: (rule.detail)(worst, &lasted),
                ongoing: b + 1 == points.len(),
                start,
                end,
            }
        })
        .collect()
}

/// Several series added point by point (read + write IOPS). Points missing from any series are left out.
pub fn sum_series(metric: &str, label: &str, unit: CloudUnit, parts: &[&CloudSeries]) -> Option<CloudSeries> {
    let first = parts.first()?;
    let points: Vec<CloudPoint> = first
        .points
        .iter()
        .filter_map(|p| {
            let mut value = p.value;
            for other in &parts[1..] {
                value += other.points.iter().find(|o| o.at == p.at)?.value;
            }
            Some(CloudPoint { at: p.at.clone(), value })
        })
        .collect();
    (!points.is_empty()).then(|| CloudSeries::new(metric, label, unit, points))
}

/// "CPU credits ran out once (1 h 10 min) and storage answered slowly twice." or "Nothing held the server back."
pub fn summarize(episodes: &[ThrottleEpisode], span: &str) -> String {
    if episodes.is_empty() {
        return format!("Nothing held the server back in the {span}: no credits ran out and no limit was reached.");
    }
    let mut kinds: Vec<ThrottleKind> = Vec::new();
    for e in episodes {
        if !kinds.contains(&e.kind) {
            kinds.push(e.kind);
        }
    }
    let parts: Vec<String> = kinds
        .iter()
        .map(|kind| {
            let of_kind: Vec<&ThrottleEpisode> = episodes.iter().filter(|e| e.kind == *kind).collect();
            let times = match of_kind.len() {
                1 => "once".to_string(),
                2 => "twice".to_string(),
                n => format!("{n} times"),
            };
            let total: f64 =
                of_kind.iter().map(|e| (epoch(&e.end).unwrap_or(0.0) - epoch(&e.start).unwrap_or(0.0)).max(0.0)).sum();
            format!(
                "{} {times} ({}{})",
                lower_first(&of_kind[0].title),
                crate::format::duration(total * 1000.0),
                if of_kind.iter().any(|e| e.ongoing) { ", still going" } else { "" }
            )
        })
        .collect();
    let joined = match parts.as_slice() {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => String::new(),
    };
    format!("In the {span}: {joined}.")
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if !text.starts_with("CPU") && !text.starts_with("IOPS") && !text.starts_with("EBS") => {
            c.to_lowercase().collect::<String>() + chars.as_str()
        }
        Some(c) => c.to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// vCPUs of an RDS instance class (`db.m6g.2xlarge` → 8). Burstable and small classes have 2.
pub fn rds_vcpus(class: &str) -> Option<f64> {
    let size = class.rsplit('.').next()?;
    let n = match size {
        "micro" | "small" | "medium" | "large" => 2.0,
        "xlarge" => 4.0,
        _ => size.strip_suffix("xlarge")?.parse::<f64>().ok()? * 4.0,
    };
    Some(n)
}

/// The CPU an RDS burstable class (t2, t3, t4g) may use without spending credits, as a share of all its vCPUs.
pub fn rds_cpu_baseline(class: &str) -> Option<f64> {
    let mut parts = class.split('.');
    let (Some("db"), Some(family), Some(size)) = (parts.next(), parts.next(), parts.next()) else { return None };
    if !family.starts_with('t') {
        return None;
    }
    let pct = match (family, size) {
        (_, "micro") => 10.0,
        ("t2", "small") => 20.0,
        ("t2", "medium") => 20.0,
        ("t2", "large") => 30.0,
        ("t2", "xlarge") => 22.5,
        ("t2", "2xlarge") => 17.0,
        (_, "small" | "medium") => 20.0,
        (_, "large") => 30.0,
        (_, "xlarge" | "2xlarge") => 40.0,
        _ => return None,
    };
    Some(pct)
}

/// What a gp2 volume sustains without burst credits: 3 IOPS per GB, at least 100, at most 16,000.
pub fn gp2_baseline_iops(allocated_gb: f64) -> f64 {
    (allocated_gb * 3.0).clamp(100.0, 16_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(values: &[f64]) -> CloudSeries {
        let points = values
            .iter()
            .enumerate()
            .map(|(i, v)| CloudPoint { at: format!("2026-10-11T12:{:02}:00Z", i * 5), value: *v })
            .collect();
        CloudSeries::new("CPUCreditBalance", "CPU credits", CloudUnit::Credits, points)
    }

    #[test]
    fn credit_balances_hitting_zero_are_episodes() {
        let detail = |worst: f64, lasted: &str| format!("balance {worst} for {lasted}");
        let rule = Rule {
            metric: "CPUCreditBalance",
            kind: ThrottleKind::CpuCredits,
            threshold: 0.5,
            floor: true,
            min_points: 2,
            title: "CPU credits ran out",
            detail: &detail,
        };
        // 0 0 1 0 is one episode (a single recovered point doesn't split it); a lone 0 later is too short.
        let s = series(&[40.0, 0.0, 0.0, 1.0, 0.0, 30.0, 50.0, 0.0, 60.0]);
        let found = episodes(&s, 300, &rule);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].start, "2026-10-11T12:05:00Z");
        assert_eq!(found[0].end, "2026-10-11T12:25:00Z");
        assert_eq!(found[0].detail, "balance 0 for 20 min");
        assert!(!found[0].ongoing);
        let summary = summarize(&found, "last 24 hours");
        assert_eq!(summary, "In the last 24 hours: CPU credits ran out once (20 min).");
        let events = CloudSignals {
            provider: CloudProvider::Aws,
            service: "Amazon RDS for PostgreSQL".into(),
            resource: "shop".into(),
            tier: None,
            start: "2026-10-11T12:00:00Z".into(),
            end: "2026-10-11T13:00:00Z".into(),
            period_seconds: 300,
            series: vec![s],
            episodes: found,
            summary,
            notes: vec![],
            fetched_at: "2026-10-11T13:00:00Z".into(),
        }
        .events();
        assert_eq!(events[0].kind, EventKind::Throttle);
        assert_eq!(events[0].until.as_deref(), Some("2026-10-11T12:25:00Z"));
    }

    #[test]
    fn instance_classes() {
        assert_eq!(rds_vcpus("db.t3.medium"), Some(2.0));
        assert_eq!(rds_vcpus("db.m6g.2xlarge"), Some(8.0));
        assert_eq!(rds_vcpus("db.r6i.xlarge"), Some(4.0));
        assert_eq!(rds_cpu_baseline("db.t3.medium"), Some(20.0));
        assert_eq!(rds_cpu_baseline("db.t4g.large"), Some(30.0));
        assert_eq!(rds_cpu_baseline("db.m5.large"), None);
        assert_eq!(gp2_baseline_iops(20.0), 100.0);
        assert_eq!(gp2_baseline_iops(200.0), 600.0);
    }

    #[test]
    fn sums_line_up_by_time() {
        let a = series(&[1.0, 2.0, 3.0]);
        let mut b = series(&[10.0, 20.0]);
        b.points.remove(0);
        let total = sum_series("IOPS", "IOPS", CloudUnit::Iops, &[&a, &b]).unwrap();
        assert_eq!(total.points.len(), 1);
        assert_eq!(total.points[0].value, 22.0);
    }
}
