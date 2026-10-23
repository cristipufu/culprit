//! Saved analysis and derived report views. This service can read the report store and run checks, but has no
//! provider, connection configuration or secret store. Workload windows and exact report ids stay part of each read.

use crate::{ReportSummary, analysis, export, reports, store::Paths, trends};
use culprit_core::{Check, Drift, Error, Report, Result};
use std::sync::Arc;

pub(crate) struct Reports<'a> {
    pub paths: &'a Paths,
    pub checks: &'a [Arc<dyn Check>],
}

impl Reports<'_> {
    pub(crate) async fn background<T: Send + 'static>(
        &self,
        read: impl for<'a> FnOnce(Reports<'a>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let paths = Paths { config: self.paths.config.clone(), data: self.paths.data.clone() };
        let checks = self.checks.to_vec();
        crate::run_blocking(move || read(Reports { paths: &paths, checks: &checks })).await
    }

    /// Re-run the checks on a snapshot or report file, so improved checks apply to old scans and shared snapshots.
    pub(crate) fn analyze_file(&self, path: &std::path::Path) -> Result<Report> {
        let mut input = reports::load_analysis_input(path)?;
        if let Some(dir) = path.parent() {
            reports::rebase_on_scans(dir, &mut input.snapshot);
        }
        Ok(self.reread(input))
    }

    /// A saved scan as this version of Culprit reads it: its plans parsed again from the text they were captured as
    /// ([`crate::plans::reparse_saved`]) and its checks run again, so a fix to a parser or a check reaches the scans saved
    /// before it, and what a person reads in an old scan is what a scan taken now would say about the same data.
    fn reread(&self, input: analysis::Input) -> Report {
        analysis::reread(input, self.checks)
    }

    /// The newest saved report, or the newest one scanned with a window of `window_minutes` when given, read the way a
    /// scan taken now would read the same data ([`crate::Workspace::analyze_file`]).
    pub(crate) fn latest_report(&self, name: &str, window_minutes: Option<u32>) -> Result<Option<Report>> {
        let dir = self.paths.reports_dir(name);
        let Some(summary) = reports::latest_summary(&dir, window_minutes) else { return Ok(None) };
        let mut input = reports::load_analysis_input(&dir.join(format!("{}.json", summary.id)))?;
        reports::rebase_on_scans(&dir, &mut input.snapshot);
        Ok(Some(self.reread(input)))
    }

    /// The newest saved snapshot for `name`, without re-parsing execution plans or re-running the check pipeline.
    pub(crate) fn latest_snapshot(&self, name: &str) -> Result<Option<culprit_core::Snapshot>> {
        let dir = self.paths.reports_dir(name);
        let Some(summary) = reports::latest_summary(&dir, None) else { return Ok(None) };
        reports::load_snapshot(&dir.join(format!("{}.json", summary.id))).map(Some)
    }

    pub(crate) fn report_history(&self, name: &str) -> Result<Vec<ReportSummary>> {
        reports::list(&self.paths.reports_dir(name))
    }

    /// The digest of a scan: `report` (a report or snapshot file) or the connection's latest saved scan, compared with
    /// `since` (a file) or the saved scan before it. Both are re-checked with the current checks, so a finding means
    /// the same thing on each side. `top` caps each list; `top_queries` is how many of the heaviest queries it lists.
    pub(crate) fn digest(
        &self,
        name: Option<&str>,
        report: Option<&std::path::Path>,
        since: Option<&std::path::Path>,
        top: usize,
        top_queries: usize,
    ) -> Result<culprit_core::Digest> {
        // A digest is posted where the database's data must not go: every report it reads is checked again from its
        // redacted snapshot, so titles, headlines and details can only quote SQL the lexer has already redacted.
        let recheck = |input: analysis::Input| analysis::shared(input, self.checks);
        // Plans are read again from their text before the redaction drops it.
        let load = |name: &str, id: &str| {
            let dir = self.paths.reports_dir(name);
            reports::load_analysis_input(&dir.join(format!("{id}.json"))).map(|mut input| {
                reports::rebase_on_scans(&dir, &mut input.snapshot);
                input
            })
        };
        let load_file = |path: &std::path::Path| -> Result<analysis::Input> {
            let mut input = reports::load_analysis_input(path)?;
            if let Some(dir) = path.parent() {
                reports::rebase_on_scans(dir, &mut input.snapshot);
            }
            Ok(input)
        };
        let history = match (name, report) {
            (Some(name), None) => reports::list(&self.paths.reports_dir(name))?,
            _ => Vec::new(),
        };
        let mut current = match (report, name) {
            (Some(path), _) => load_file(path)?,
            (None, Some(name)) => {
                let latest = history.first().ok_or_else(|| {
                    Error::not_found(format!("No saved report for '{name}'"))
                        .with_hint(format!("Run `culprit scan {name}` first"))
                })?;
                load(name, &latest.id)?
            }
            (None, None) => {
                return Err(Error::invalid("Name a connection or pass --report <file>")
                    .with_hint("culprit digest shop, or culprit digest --report report.json --since previous.json"));
            }
        };
        // A scan without an earlier one to compare lock escalations with (a scheduled job keeps no scans) compares
        // them with the baseline's counts. When `--since` is explicitly supplied, always compare against that baseline.
        let unbased = current.snapshot.escalations_before.is_none();
        // A previous digest is the baseline a scheduled job keeps: it holds finding ids and titles, no statement text.
        if let Some(path) = since
            && let Some(previous) = read_digest(path)?
        {
            current.snapshot.escalations_before =
                culprit_core::EscalationBaseline::from_digest(&previous, &current.snapshot);
            let current = recheck(current);
            return Ok(with_details(&current, culprit_checks::digest_since(&current, &previous, top, top_queries)));
        }
        // The scan before, over the same window: a 7-day scan next to a 24-hour one would show findings coming and
        // going that only the window explains.
        let previous = match (since, name, report) {
            (Some(path), _, _) => Some(recheck(load_file(path)?)),
            (None, Some(name), None) => {
                let window = history.first().and_then(|r| r.window());
                match history.iter().skip(1).find(|r| r.window() == window) {
                    Some(summary) => Some(recheck(load(name, &summary.id)?)),
                    None => None,
                }
            }
            _ => None,
        };
        if (unbased || since.is_some())
            && let Some(previous) = &previous
        {
            current.snapshot.escalations_before = previous
                .snapshot
                .report_comparison(&current.snapshot)
                .allows_comparison()
                .then(|| culprit_core::EscalationBaseline::from_scan(&previous.snapshot, &current.snapshot))
                .flatten();
        }
        if current.snapshot.evictions_before.is_none()
            && let Some(previous) = &previous
        {
            current.snapshot.evictions_before = culprit_core::CountBaseline::from_setting(
                &previous.snapshot,
                &current.snapshot,
                trends::EVICTIONS_SETTING,
            );
        }
        let current = recheck(current);
        Ok(with_details(&current, culprit_checks::digest(&current, previous.as_ref(), top, top_queries)))
    }

    /// The index advisor's plan for a connection, from its latest saved report re-checked with the current checks (so
    /// advice from an older report still links to today's findings).
    pub(crate) fn index_advice(&self, name: &str) -> Result<culprit_core::IndexAdvice> {
        let report = self.latest_report(name, None)?.ok_or_else(|| {
            Error::not_found(format!("No saved report for '{name}'"))
                .with_hint(format!("Run `culprit scan {name}` first"))
        })?;
        Ok(culprit_checks::index_advice(&report))
    }

    /// The index advisor's plan for a report already in hand (a saved file, a fixture).
    pub(crate) fn index_advice_for(&self, report: Report) -> culprit_core::IndexAdvice {
        culprit_checks::index_advice(&self.reread(report.into()))
    }

    /// Advice for the exact scan displayed by a client, including its workload window. Never falls back to a
    /// different scan; ids must belong to this connection's report directory.
    pub(crate) fn index_advice_at(&self, name: &str, report_id: &str) -> Result<culprit_core::IndexAdvice> {
        let report = self.report_at(name, report_id)?;
        Ok(culprit_checks::index_advice(&report))
    }

    pub(crate) fn report_at(&self, name: &str, report_id: &str) -> Result<Report> {
        let dir = self.paths.reports_dir(name);
        if !reports::report_ids(&dir).iter().any(|id| id == report_id) {
            return Err(Error::not_found(format!("No saved report '{report_id}' for '{name}'")));
        }
        self.analyze_file(&dir.join(format!("{report_id}.json")))
    }

    /// What differs between the latest scans of two connections: tables, columns, indexes, settings, version.
    pub(crate) fn drift(&self, a: &str, b: &str) -> Result<Drift> {
        let load = |name: &str| -> Result<Report> {
            let mut report = self.latest_report(name, None)?.ok_or_else(|| {
                Error::not_found(format!("No saved report for '{name}'"))
                    .with_hint(format!("Run `culprit scan {name}` first"))
            })?;
            report.snapshot.connection = name.to_string();
            Ok(report)
        };
        drift_reports(&load(a)?, &load(b)?)
    }

    pub(crate) fn plan_report(&self, name: &str, report_id: Option<&str>) -> Result<Option<Report>> {
        let mut report = match report_id {
            Some(id) => self.report_at(name, id).map(Some),
            None => self.latest_report(name, None),
        }?;
        // Contextualizing a plan needs the prepared picture, not retained copies held across a database await.
        if let Some(report) = &mut report {
            report.evidence = None;
        }
        Ok(report)
    }

    /// Where the latest scan's database time went, by wait category, with the queries behind each. `None` when the scan
    /// recorded no waits (PostgreSQL without track_io_timing, logins that can't read wait statistics).
    pub(crate) fn waits(&self, name: &str, window_minutes: Option<u32>) -> Result<Option<culprit_core::WaitsReport>> {
        let report = self.latest_report(name, window_minutes)?.ok_or_else(|| {
            Error::not_found(format!("No report for {name} yet")).with_hint("Scan it first: culprit scan <connection>")
        })?;
        Ok(explain_waits(&report.snapshot))
    }
}

/// Each top finding in full, for a consumer that posts the details (a thread per finding). `report` is checked from a
/// redacted snapshot (see [`Reports::digest`]).
fn with_details(report: &Report, mut digest: culprit_core::Digest) -> culprit_core::Digest {
    for item in &mut digest.top {
        item.detail =
            report.findings.iter().find(|f| f.id == item.id).map(|f| export::finding_detail_shared(report, f));
    }
    digest
}

/// `path` as a digest (`culprit digest` output), or `None` when it is a report or snapshot instead.
fn read_digest(path: &std::path::Path) -> Result<Option<culprit_core::Digest>> {
    let bytes = std::fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
    if value.get("snapshot").is_some() || value.get("counts").is_none() || value.get("findings").is_none() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| Error::invalid(format!("{} is not a Culprit digest: {e}", path.display())))
}

pub(crate) fn drift_reports(a: &Report, b: &Report) -> Result<Drift> {
    let (ea, eb) = (a.snapshot.server.engine, b.snapshot.server.engine);
    if ea != eb {
        return Err(Error::invalid(format!(
            "'{}' is {ea} and '{}' is {eb}; drift compares databases of the same engine",
            a.snapshot.connection, b.snapshot.connection
        )));
    }
    Ok(culprit_checks::drift(a, b))
}

/// The dashboard for a report already in hand (a saved file, a fixture), without a live read.
pub(crate) fn dashboard_report(report: &Report) -> culprit_core::Dashboard {
    culprit_checks::dashboard(report)
}

/// Where a snapshot's database time went, by wait category with the queries behind each (saved files, fixtures).
pub(crate) fn explain_waits(snapshot: &culprit_core::Snapshot) -> Option<culprit_core::WaitsReport> {
    culprit_checks::waits::explain(snapshot)
}
