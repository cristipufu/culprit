//! Reports on disk, one JSON file per scan: `<data>/reports/<connection>/<snapshot id>.json`, plus
//! `<snapshot id>.counters.json` with the running totals that trends for cumulative sources are built from.

use std::fs;
use std::path::{Path, PathBuf};

use culprit_core::{Error, Report, Result};

use crate::private_fs;
use crate::trends::{self, Counters};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ReportSummary {
    pub id: String,
    pub taken_at: String,
    pub opportunities: u32,
    pub headline: String,
    #[serde(default)]
    pub window_days: Option<u32>,
    /// The server as the scan saw it ("MariaDB 11.8.9"), so a list of connections can tell MariaDB from MySQL.
    #[serde(default)]
    pub product: Option<String>,
    /// Why the scan has no query history ("Query Store is OFF on this database."); `None` when it has one.
    #[serde(default)]
    pub no_workload: Option<String>,
}

fn summary_path(dir: &Path, window_days: Option<u32>) -> PathBuf {
    match window_days {
        Some(days) => dir.join(format!("latest-{days}d.json")),
        None => dir.join("latest.json"),
    }
}

fn summary_of(report: &Report) -> ReportSummary {
    ReportSummary {
        id: report.snapshot.id.clone(),
        taken_at: report.snapshot.taken_at.clone(),
        opportunities: report.summary.opportunities,
        headline: report.summary.headline.clone(),
        window_days: report.snapshot.options.as_ref().map(|o| o.window_days),
        product: Some(report.snapshot.server.product.clone()),
        no_workload: report.snapshot.no_workload_reason(),
    }
}

pub fn save(dir: &Path, report: &Report) -> Result<()> {
    private_fs::create_dir(dir).map_err(|e| Error::internal(format!("{}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.json", report.snapshot.id));
    let json = serde_json::to_vec(report).map_err(|e| Error::internal(e.to_string()))?;
    write_atomic(&path, &json)?;
    if let Some(counters) = Counters::of(report) {
        let json = serde_json::to_vec(&counters).map_err(|e| Error::internal(e.to_string()))?;
        write_atomic(&counters_path(dir, &report.snapshot.id), &json)?;
    }
    let summary = summary_of(report);
    let json = serde_json::to_vec_pretty(&summary).map_err(|e| Error::internal(e.to_string()))?;
    write_atomic(&summary_path(dir, None), &json)?;
    if let Some(days) = summary.window_days {
        write_atomic(&summary_path(dir, Some(days)), &json)?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    private_fs::write_atomic(path, bytes).map_err(|e| Error::internal(format!("{}: {e}", path.display())))
}

const COUNTERS_SUFFIX: &str = ".counters.json";
/// Reports from before counters files existed are read in full once; cap that work for a single scan.
const MAX_BACKFILL: usize = 60;

fn counters_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}{COUNTERS_SUFFIX}"))
}

/// `<snapshot id>.json`: ids are UTC timestamps, so anything else in the directory (latest pointers, sidecars, marks,
/// the cloud cache) is not a report.
fn is_report_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.ends_with(".json")
        && name.starts_with(|c: char| c.is_ascii_digit())
        && !name.ends_with(COUNTERS_SUFFIX)
        && !name.ends_with(crate::history::EVENTS_SUFFIX)
}

/// Snapshot ids of every saved report, oldest first.
pub(crate) fn report_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_report_file(p))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    ids.sort();
    ids
}

/// Running totals of every saved scan in the last [`trends::HISTORY_DAYS`] days, oldest first. Writes the counters file
/// for older reports that don't have one yet.
pub fn counters_history(dir: &Path, now: &str) -> Vec<Counters> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let oldest = trends::epoch(now).map(|t| t - trends::HISTORY_DAYS * 86_400.0).unwrap_or(f64::MIN);
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    let mut out = Vec::new();
    let mut backfilled = 0;
    for path in paths.iter().filter(|p| is_report_file(p)) {
        let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // Snapshot ids are their UTC timestamps (`20261011T120000.000Z`); skip old ones without opening them.
        if id_epoch(&id).is_some_and(|t| t < oldest) {
            continue;
        }
        let sidecar = counters_path(dir, &id);
        if let Some(c) = fs::read(&sidecar).ok().and_then(|b| serde_json::from_slice::<Counters>(&b).ok()) {
            out.push(c);
            continue;
        }
        if backfilled >= MAX_BACKFILL {
            continue;
        }
        backfilled += 1;
        if let Some(c) = load(path).ok().as_ref().and_then(Counters::of) {
            if let Ok(json) = serde_json::to_vec(&c) {
                let _ = write_atomic(&sidecar, &json);
            }
            out.push(c);
        }
    }
    out.retain(|c| trends::epoch(&c.taken_at).is_some_and(|t| t >= oldest));
    out.sort_by(|a, b| a.taken_at.cmp(&b.taken_at));
    out
}

pub(crate) fn id_epoch(id: &str) -> Option<f64> {
    let t = chrono::NaiveDateTime::parse_from_str(id.get(..15)?, "%Y%m%dT%H%M%S").ok()?;
    Some(t.and_utc().timestamp() as f64)
}

pub fn latest_summary(dir: &Path, window_days: Option<u32>) -> Option<ReportSummary> {
    let bytes = fs::read(summary_path(dir, window_days)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The most recent report, or the most recent one taken with a given window.
pub fn latest(dir: &Path, window_days: Option<u32>) -> Result<Option<Report>> {
    let Some(summary) = latest_summary(dir, window_days) else {
        return Ok(None);
    };
    load(&dir.join(format!("{}.json", summary.id))).map(Some)
}

pub fn load(path: &Path) -> Result<Report> {
    let bytes = fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|e| {
        // `latest.json` is the one-line summary of the newest scan, not the scan itself.
        if serde_json::from_slice::<ReportSummary>(&bytes).is_ok() {
            return Error::invalid(format!("{} is a scan summary, not a report", path.display())).with_hint(
                "Pass the connection's name to use its latest scan, or a report file (`<snapshot id>.json`) from the same folder.",
            );
        }
        Error::invalid(format!("{} is not a Culprit report: {e}", path.display()))
    })
}

pub fn list(dir: &Path) -> Result<Vec<ReportSummary>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<ReportSummary> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_report_file(p))
        .filter_map(|p| load(&p).ok())
        .map(|r| summary_of(&r))
        .collect();
    out.sort_by(|a, b| b.taken_at.cmp(&a.taken_at));
    Ok(out)
}
