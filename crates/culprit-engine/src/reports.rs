//! Reports on disk, one JSON file per scan: `<data>/reports/<connection>/<snapshot id>.json`, plus
//! `<snapshot id>.counters.json` with the running totals that trends for cumulative sources are built from.

use std::fs;
use std::path::{Path, PathBuf};

use culprit_core::{Error, MINUTES_PER_DAY, Report, Result, window_label};

use crate::private_fs;
use crate::trends::{self, Counters};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

mod storage;

/// The prepared report for clients. Retained replay/collection copies stay in the report store.
pub fn display(report: &Report) -> impl Serialize + '_ {
    #[derive(Serialize)]
    struct View<'a> {
        snapshot: &'a culprit_core::Snapshot,
        findings: &'a [culprit_core::Finding],
        locked: &'a [culprit_core::LockedCheck],
        summary: &'a culprit_core::Summary,
        assessments: &'a [culprit_core::CheckAssessment],
        #[serde(skip_serializing_if = "<[culprit_core::RefinementDecision]>::is_empty")]
        refinement: &'a [culprit_core::RefinementDecision],
        #[serde(skip_serializing_if = "Option::is_none")]
        provenance: &'a Option<culprit_core::AnalysisProvenance>,
    }
    let Report { snapshot, findings, locked, summary, assessments, refinement, provenance, evidence: _ } = report;
    View { snapshot, findings, locked, summary, assessments, refinement, provenance }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ReportSummary {
    pub id: String,
    pub taken_at: String,
    pub opportunities: u32,
    pub headline: String,
    /// The scan's window in whole days; absent when it was hours (`window_minutes`).
    #[serde(default)]
    pub window_days: Option<u32>,
    /// The scan's window in minutes. Absent from summaries written before windows could be hours.
    #[serde(default)]
    pub window_minutes: Option<u32>,
    /// The server as the scan saw it ("MariaDB 11.8.9"), so a list of connections can tell MariaDB from MySQL.
    #[serde(default)]
    pub product: Option<String>,
    /// Why the scan has no query history ("Query Store is OFF on this database."); `None` when it has one.
    #[serde(default)]
    pub no_workload: Option<String>,
}

impl ReportSummary {
    /// The scan's window in minutes, for summaries from before hour windows too.
    pub fn window(&self) -> Option<u32> {
        self.window_minutes.or_else(|| self.window_days.map(|days| days.saturating_mul(MINUTES_PER_DAY)))
    }
}

/// The pointer to the latest scan with a window (`latest-7d.json`, `latest-6h.json`), or to the latest of any.
fn summary_path(dir: &Path, window_minutes: Option<u32>) -> PathBuf {
    match window_minutes {
        Some(minutes) => dir.join(format!("latest-{}.json", window_label(minutes))),
        None => dir.join("latest.json"),
    }
}

fn summary_of(report: &Report) -> ReportSummary {
    ReportSummary {
        id: report.snapshot.id.clone(),
        taken_at: report.snapshot.taken_at.clone(),
        opportunities: report.summary.opportunities,
        headline: report.summary.headline.clone(),
        window_days: report.snapshot.options.as_ref().filter(|o| o.window_minutes.is_none()).map(|o| o.window_days),
        window_minutes: report.snapshot.options.as_ref().map(|o| o.window_in_minutes()),
        product: Some(report.snapshot.server.product.clone()),
        no_workload: report.snapshot.no_workload_reason(),
    }
}

pub fn save(dir: &Path, report: &Report) -> Result<()> {
    private_fs::create_dir(dir).map_err(|e| Error::internal(format!("{}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.json", report.snapshot.id));
    let json = storage::encode(report).map_err(|e| Error::internal(e.to_string()))?;
    write_atomic(&path, &json)?;
    if let Some(counters) = Counters::of(report) {
        let json = serde_json::to_vec(&counters).map_err(|e| Error::internal(e.to_string()))?;
        write_atomic(&counters_path(dir, &report.snapshot.id), &json)?;
    }
    let summary = summary_of(report);
    let json = serde_json::to_vec_pretty(&summary).map_err(|e| Error::internal(e.to_string()))?;
    write_atomic(&dir.join(format!("{}.summary.json", report.snapshot.id)), &json)?;
    write_atomic(&summary_path(dir, None), &json)?;
    if let Some(minutes) = summary.window() {
        write_atomic(&summary_path(dir, Some(minutes)), &json)?;
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
        && !name.ends_with(".summary.json")
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
        if let Some(mut c) = fs::read(&sidecar).ok().and_then(|b| serde_json::from_slice::<Counters>(&b).ok()) {
            c.bind_artifact(&id);
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

/// A saved scan read again with what the scans saved beside it in `dir` say: pg_stat_statements' evictions against the
/// oldest scan a day or more back ([`evictions_baseline`]), the baseline a scan taken now would use. A retained
/// scan-time baseline remains evidence when its original report was pruned; absence of that file does not erase
/// the measurements stored in this scan. Writes nothing.
pub fn rebase_on_scans(dir: &Path, snapshot: &mut culprit_core::Snapshot) {
    snapshot.ensure_measurement();
    if let Some(baseline) = evictions_baseline(dir, snapshot) {
        snapshot.evictions_before = Some(baseline);
    }
    if snapshot.indexes.iter().any(|i| i.contention.is_some())
        || !snapshot.operations.counters.is_empty()
        || snapshot.operations.sqlite.is_some()
    {
        let now = trends::epoch(&snapshot.taken_at);
        // Only a few adjacent scans are needed, and every candidate must precede this report. Reopening a saved
        // report never compares it with scans taken later or with another database. This path writes nothing.
        for id in report_ids(dir)
            .into_iter()
            .rev()
            .filter(|id| {
                id_epoch(id).zip(now).is_some_and(|(at, now)| at < now && at >= now - trends::HISTORY_DAYS * 86_400.0)
            })
            .take(5)
        {
            let Ok(previous) = load_snapshot(&dir.join(format!("{id}.json"))) else { continue };
            if !crate::history::same_database(&previous, snapshot)
                || trends::epoch(&previous.taken_at).zip(now).is_none_or(|(at, now)| at >= now)
            {
                continue;
            }
            snapshot.escalations_before = culprit_core::EscalationBaseline::from_scan(&previous, snapshot);
            culprit_core::operations::compare(&previous, snapshot);
            if let Some(baseline) = culprit_core::ContentionBaseline::from_scan(&previous, snapshot) {
                snapshot.contention_before = Some(baseline);
            } else {
                snapshot.contention_before = None;
            }
            // A reset or unknown start in the closest matching scan cannot be bypassed by an older counter.
            break;
        }
    }
    rebuild_timeline(dir, snapshot);
}

/// A saved scan's timeline from Culprit's scans built again from the counters the scans before it saved beside it in
/// `dir`, with this version's reading of each step (`trends::from_scans`): a step a saved scan misread (a statement
/// that outran the whole database, everything since a reset in one bucket) reads as the counters say, and a step they
/// can't account for in full stays marked so. The saved timeline stays where no earlier counters are kept, and only
/// counters files already saved are read: nothing is written.
pub fn rebuild_timeline(dir: &Path, snapshot: &mut culprit_core::Snapshot) {
    let evictions = trends::evictions(snapshot);
    let (database, taken_at) = (snapshot.server.database.clone(), snapshot.taken_at.clone());
    let Some(workload) = snapshot.workload.as_mut().filter(|w| {
        w.is_cumulative() && w.timeline.as_ref().is_none_or(|t| t.source == culprit_core::TimelineSource::Scans)
    }) else {
        return;
    };
    if workload.timeline.is_none() {
        // No captured timeline backs these derived references or query buckets.
        workload.history_inputs.retain(|r| r.purpose != culprit_core::BaselinePurpose::WorkloadTrend);
        for q in &mut workload.queries {
            q.trend = None;
        }
    }
    let Some(now) = trends::epoch(&taken_at) else { return };
    let oldest = now - trends::HISTORY_DAYS * 86_400.0;
    let history: Vec<Counters> = report_ids(dir)
        .into_iter()
        .filter(|id| id_epoch(id).is_some_and(|t| t >= oldest - 1.0 && t < now))
        .filter_map(|id| {
            let mut counters = fs::read(counters_path(dir, &id))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Counters>(&bytes).ok())?;
            counters.bind_artifact(&id);
            Some(counters)
        })
        .collect();
    if history.is_empty() {
        return;
    }
    let saved = workload.timeline.take();
    let trends: Vec<Option<culprit_core::QueryTrend>> = workload.queries.iter_mut().map(|q| q.trend.take()).collect();
    // from_scans replaces WorkloadTrend references only after finding comparable points. If it cannot rebuild,
    // the captured timeline, buckets and their retained references still describe the original observation.
    trends::from_scans(workload, &database, &taken_at, evictions, &history);
    if workload.timeline.is_none() {
        workload.timeline = saved;
        for (q, trend) in workload.queries.iter_mut().zip(trends) {
            q.trend = trend;
        }
    }
}

/// Saved reports read for their eviction count where their counters file was written before it kept one, at most.
const MAX_EVICTION_READS: usize = 10;

/// [`trends::evictions_baseline`] over the scans saved in `dir` (the last [`trends::HISTORY_DAYS`] days, a day or more
/// before `snapshot`), oldest first, stopping at the first the count continues: the oldest. A counters file written
/// before counters kept pg_stat_statements' eviction count gets it from its report, a few at most. Writes nothing.
pub fn evictions_baseline(dir: &Path, snapshot: &culprit_core::Snapshot) -> Option<culprit_core::CountBaseline> {
    trends::evictions(snapshot)?;
    let at = trends::epoch(&snapshot.taken_at)?;
    let newest = at - culprit_core::PER_DAY_MIN_HOURS * 3600.0;
    let oldest = at - trends::HISTORY_DAYS * 86_400.0;
    let mut history = Vec::new();
    let mut reads = 0;
    for id in report_ids(dir) {
        // Ids are UTC timestamps to the millisecond, so a scan within a second of the cut is read and judged by its own.
        if id_epoch(&id).is_none_or(|t| t < oldest - 1.0 || t > newest + 1.0) {
            continue;
        }
        // A report without a counters file has none to give: every scan writes one for those that do
        // ([`counters_history`]).
        let saved = fs::read(counters_path(dir, &id)).ok().and_then(|b| serde_json::from_slice::<Counters>(&b).ok());
        let Some(mut counters) = saved else { continue };
        counters.bind_artifact(&id);
        if counters.evictions.is_none() && reads < MAX_EVICTION_READS {
            reads += 1;
            counters.evictions =
                load(&dir.join(format!("{id}.json"))).ok().and_then(|r| trends::evictions(&r.snapshot));
        }
        history.push(counters);
        if let Some(baseline) = trends::evictions_baseline(&history, snapshot) {
            return Some(baseline);
        }
    }
    None
}

pub(crate) fn id_epoch(id: &str) -> Option<f64> {
    let t = chrono::NaiveDateTime::parse_from_str(id.get(..15)?, "%Y%m%dT%H%M%S").ok()?;
    Some(t.and_utc().timestamp() as f64)
}

pub fn latest_summary(dir: &Path, window_minutes: Option<u32>) -> Option<ReportSummary> {
    let bytes = fs::read(summary_path(dir, window_minutes)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The most recent report, or the most recent one taken with a given window (in minutes).
pub fn latest(dir: &Path, window_minutes: Option<u32>) -> Result<Option<Report>> {
    let Some(summary) = latest_summary(dir, window_minutes) else {
        return Ok(None);
    };
    load(&dir.join(format!("{}.json", summary.id))).map(Some)
}

pub fn load(path: &Path) -> Result<Report> {
    let bytes = fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    storage::decode(&bytes).map_err(|e| {
        // `latest.json` is the one-line summary of the newest scan, not the scan itself.
        if serde_json::from_slice::<ReportSummary>(&bytes).is_ok() {
            return Error::invalid(format!("{} is a scan summary, not a report", path.display())).with_hint(
                "Pass the connection's name to use its latest scan, or a report file (`<snapshot id>.json`) from the same folder.",
            );
        }
        Error::invalid(format!("{} is not a Culprit report: {e}", path.display()))
    })
}

/// The prepared picture in a snapshot or report file, used to compare historical measurements.
/// Analysis replay separately reads retained original input through `load_analysis_input`.
pub fn load_snapshot(path: &Path) -> Result<culprit_core::Snapshot> {
    let bytes = fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
    let snapshot = if value.get("snapshot").is_some() { value["snapshot"].take() } else { value };
    serde_json::from_value(snapshot)
        .map_err(|e| Error::invalid(format!("{} is not a Culprit snapshot or report: {e}", path.display())))
}

/// Analysis reads retained input, while history's `load_snapshot` continues to read the scan's prepared picture.
/// A wrapper containing only `snapshot` remains accepted, as it was before reports retained replay evidence.
pub(crate) fn load_analysis_input(path: &Path) -> Result<crate::analysis::Input> {
    let bytes = fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
    if value.get("snapshot").is_some() && value.get("findings").is_some() && value.get("summary").is_some() {
        return storage::expand(&mut value)
            .and_then(|()| serde_json::from_value::<Report>(value))
            .map(|mut report| {
                storage::share_native_text(&mut report);
                crate::analysis::Input::from(report)
            })
            .map_err(|e| Error::invalid(format!("{} is not a Culprit report: {e}", path.display())));
    }
    let wrapped = value.get("snapshot").is_some();
    let snapshot = if wrapped { value["snapshot"].take() } else { value };
    let snapshot: culprit_core::Snapshot = serde_json::from_value(snapshot)
        .map_err(|e| Error::invalid(format!("{} is not a Culprit snapshot or report: {e}", path.display())))?;
    let mut input = crate::analysis::Input::from(snapshot);
    if wrapped {
        input.origin = culprit_core::AnalysisOrigin::PreparedReport;
    }
    Ok(input)
}

pub fn list(dir: &Path) -> Result<Vec<ReportSummary>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<ReportSummary> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_report_file(p))
        .filter_map(|p| {
            let id = p.file_stem()?.to_str()?;
            let sidecar = p.with_file_name(format!("{id}.summary.json"));
            fs::read(&sidecar)
                .ok()
                .and_then(|b| serde_json::from_slice::<ReportSummary>(&b).ok())
                .filter(|s| s.id == id)
                .or_else(|| summary_from_file(&p))
        })
        .collect();
    out.sort_by(|a, b| b.taken_at.cmp(&a.taken_at));
    Ok(out)
}

/// Legacy reports have no summary sidecar. Deserialize only the few fields a history row needs, skipping plans,
/// findings and retained evidence directly in the JSON parser. Listing remains read-only.
fn summary_from_file(path: &Path) -> Option<ReportSummary> {
    #[derive(Deserialize)]
    struct Header {
        snapshot: Picture,
        summary: Headline,
    }
    #[derive(Deserialize)]
    struct Picture {
        id: String,
        taken_at: String,
        options: Option<culprit_core::SnapshotOptions>,
        server: Product,
    }
    #[derive(Deserialize)]
    struct Product {
        product: String,
    }
    #[derive(Deserialize)]
    struct Headline {
        opportunities: u32,
        headline: String,
        #[serde(default)]
        no_workload: Option<String>,
    }
    let reader = std::io::BufReader::new(fs::File::open(path).ok()?);
    let h: Header = serde_json::from_reader(reader).ok()?;
    Some(ReportSummary {
        id: h.snapshot.id,
        taken_at: h.snapshot.taken_at,
        opportunities: h.summary.opportunities,
        headline: h.summary.headline,
        window_days: h.snapshot.options.as_ref().filter(|o| o.window_minutes.is_none()).map(|o| o.window_days),
        window_minutes: h.snapshot.options.as_ref().map(|o| o.window_in_minutes()),
        product: Some(h.snapshot.server.product),
        no_workload: h.summary.no_workload,
    })
}

#[cfg(test)]
mod tests {
    use culprit_core::SnapshotOptions;

    use super::*;

    fn fixture_report(id: &str, window_minutes: u32) -> Report {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/mssql-demo.report.json");
        let mut report: Report = serde_json::from_str(&fs::read_to_string(path).expect("fixture")).expect("report");
        report.snapshot.id = id.into();
        report.snapshot.options = Some(SnapshotOptions::default().with_window_minutes(window_minutes));
        report
    }

    #[test]
    fn history_rows_use_sidecars_or_stream_only_the_legacy_header() {
        let dir = std::env::temp_dir().join(format!("culprit-summary-header-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let report = fixture_report("20261021T100000.000Z", 60);
        save(&dir, &report).unwrap();
        assert_eq!(report_ids(&dir), vec![report.snapshot.id.clone()]);
        let path = dir.join(format!("{}.json", report.snapshot.id));
        let header = list(&dir).unwrap();
        // No full report deserialization is needed for a history row, even without the new sidecar.
        let mut value = serde_json::to_value(display(&report)).unwrap();
        value["findings"] = serde_json::json!("Not a typed finding array");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(list(&dir).unwrap()[0].id, header[0].id);
        fs::remove_file(dir.join(format!("{}.summary.json", report.snapshot.id))).unwrap();
        let legacy = list(&dir).unwrap();
        assert_eq!(legacy[0].id, header[0].id);
        assert_eq!(legacy[0].headline, header[0].headline);
        assert_eq!(legacy[0].window(), Some(60));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saved_lock_baselines_use_only_earlier_matching_scans_and_write_nothing() {
        let dir = std::env::temp_dir().join(format!("culprit-lock-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let make = |id: &str, at: &str| {
            let mut r = fixture_report(id, 60);
            r.snapshot.taken_at = at.into();
            r.snapshot.server.started_at = Some("2026-10-01T00:00:00Z".into());
            r.snapshot.indexes[0].contention = Some(culprit_core::IndexContention {
                row_lock_waits: 1.0,
                row_lock_wait_ms: 13_000.0,
                ..Default::default()
            });
            r
        };
        let old = make("20261018T100000", "2026-10-18T10:00:00Z");
        save(&dir, &old).unwrap();
        let future = make("20261018T130000", "2026-10-18T13:00:00Z");
        save(&dir, &future).unwrap();
        let mut wrong = make("20261018T110000", "2026-10-18T11:00:00Z");
        wrong.snapshot.server.database = "another_database".into();
        save(&dir, &wrong).unwrap();
        let contents = || -> Vec<_> {
            let mut files: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
            files.sort();
            files
                .into_iter()
                .map(|p| {
                    let b = fs::read(&p).unwrap();
                    (p, b)
                })
                .collect()
        };
        let original = contents();
        let mut current = make("20261018T120000", "2026-10-18T12:00:00Z").snapshot;
        rebase_on_scans(&dir, &mut current);
        assert_eq!(current.contention_before.as_ref().unwrap().taken_at, old.snapshot.taken_at);
        assert_eq!(current.contention_before.as_ref().unwrap().since(&current.indexes[0]).unwrap().waits, 0.0);
        assert_eq!(contents(), original);
        // The nearest matching scan has an unknown start: do not leap over it to resurrect old counters.
        let mut unknown = make("20261018T113000", "2026-10-18T11:30:00Z");
        unknown.snapshot.server.started_at = None;
        save(&dir, &unknown).unwrap();
        rebase_on_scans(&dir, &mut current);
        assert!(current.contention_before.is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_hours_scan_keeps_its_own_latest_pointer() {
        let dir = std::env::temp_dir().join(format!("culprit-reports-window-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        save(&dir, &fixture_report("20261013T080000", MINUTES_PER_DAY)).expect("save the day scan");
        save(&dir, &fixture_report("20261013T090000", 360)).expect("save the 6h scan");

        assert!(dir.join("latest-6h.json").exists());
        // The 6-hour scan doesn't take the 24-hour window's place.
        let id = |window: Option<u32>| latest(&dir, window).expect("read").expect("a report").snapshot.id;
        assert_eq!(id(Some(MINUTES_PER_DAY)), "20261013T080000");
        assert_eq!(id(Some(360)), "20261013T090000");
        assert_eq!(id(None), "20261013T090000");
        let summary = latest_summary(&dir, Some(360)).expect("6h summary");
        assert_eq!((summary.window_days, summary.window_minutes, summary.window()), (None, Some(360), Some(360)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_summary_from_before_hour_windows_still_names_its_window() {
        let old: ReportSummary =
            serde_json::from_str(r#"{"id":"x","taken_at":"t","opportunities":1,"headline":"h","window_days":7}"#)
                .expect("old summary");
        assert_eq!(old.window(), Some(7 * MINUTES_PER_DAY));
        assert_eq!(summary_path(Path::new("/r"), old.window()), Path::new("/r/latest-7d.json"));
    }
}
