//! Reports on disk, one JSON file per scan: `<data>/reports/<connection>/<snapshot id>.json`.

use std::fs;
use std::path::{Path, PathBuf};

use culprit_core::{Error, Report, Result};

use crate::private_fs;
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
    }
}

pub fn save(dir: &Path, report: &Report) -> Result<()> {
    private_fs::create_dir(dir).map_err(|e| Error::internal(format!("{}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.json", report.snapshot.id));
    let json = serde_json::to_vec(report).map_err(|e| Error::internal(e.to_string()))?;
    write_atomic(&path, &json)?;
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
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::invalid(format!("{} is not a Culprit report: {e}", path.display())))
}

pub fn list(dir: &Path) -> Result<Vec<ReportSummary>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<ReportSummary> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && p.file_name().is_some_and(|n| !n.to_string_lossy().starts_with("latest"))
        })
        .filter_map(|p| load(&p).ok())
        .map(|r| summary_of(&r))
        .collect();
    out.sort_by(|a, b| b.taken_at.cmp(&a.taken_at));
    Ok(out)
}
