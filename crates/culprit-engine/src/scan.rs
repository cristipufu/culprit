//! One scan's collection, history enrichment and persistence. This service needs a report directory and checks,
//! never connection configuration or secrets. Workspace resolves the provider and handles rejected credentials.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use culprit_core::{Check, CollectorNote, ErrorCode, Provider, Report, Result, Snapshot, SnapshotOptions};

use crate::{analysis, history, query_log, reports, trends};

pub(crate) struct QueryLog<'a> {
    pub path: &'a Path,
    pub discovered: bool,
}

pub(crate) struct Scan<'a> {
    dir: PathBuf,
    checks: &'a [Arc<dyn Check>],
    started: Instant,
    taken_at: chrono::DateTime<chrono::Utc>,
}

impl<'a> Scan<'a> {
    /// Start the clock before authentication, as the public scan operation always has.
    pub fn begin(dir: PathBuf, checks: &'a [Arc<dyn Check>]) -> Self {
        Self { dir, checks, started: Instant::now(), taken_at: chrono::Utc::now() }
    }

    /// Complete an already collected primary snapshot. Consuming the operation keeps its clock and history inputs
    /// tied to this one scan; resolving a provider or rejecting its primary read remains with the connection service.
    pub async fn finish(
        self,
        provider: &dyn Provider,
        mut snapshot: Snapshot,
        name: &str,
        options: &SnapshotOptions,
        log: Option<QueryLog<'_>>,
    ) -> Result<Report> {
        // What was asked for, recorded once for every provider: the saved report is found by its window.
        if snapshot.options.is_none() {
            snapshot.options = Some(options.clone());
        }
        if let Some(log) = log {
            query_log::attach(provider, &mut snapshot, log.path, options, log.discovered).await?;
        }
        snapshot.id = self.taken_at.format("%Y%m%dT%H%M%S%.3fZ").to_string();
        snapshot.taken_at = self.taken_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        snapshot.connection = name.to_string();
        snapshot.duration_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        snapshot.ensure_measurement();
        let dir = self.dir;
        // What the database dated itself since the previous scan (schema, settings), for the change timeline.
        let previous = reports::latest_summary(&dir, None);
        let since = previous
            .as_ref()
            .map(|p| p.taken_at.clone())
            .unwrap_or_else(|| (self.taken_at - chrono::Duration::days(trends::HISTORY_DAYS as i64)).to_rfc3339());
        // Deadlocks over the scan's window, a day at least: graphs where the engine keeps them, Postgres's counter.
        // Neither fails the scan.
        let deadlock_hours = (f64::from(options.window_in_minutes()) / 60.0).clamp(24.0, 24.0 * 30.0);
        let (deadlocks, recorded) = tokio::join!(provider.deadlocks(deadlock_hours), provider.recorded_changes(&since));
        match deadlocks {
            Ok(found) => snapshot.deadlocks = Some(found),
            Err(e) if e.code == ErrorCode::Unsupported => {}
            Err(e) => snapshot.notes.push(CollectorNote { collector: "deadlocks".into(), message: e.message }),
        }
        let recorded = match recorded {
            Ok(found) => found,
            Err(e) if e.code == ErrorCode::Unsupported => Vec::new(),
            Err(e) => {
                snapshot.notes.push(CollectorNote { collector: "changes".into(), message: e.message });
                Vec::new()
            }
        };
        // Retain the provider's observations before history dates counters, fills periods or sets aside replica use.
        let collected = snapshot.clone();
        let collection = culprit_core::CollectionProvenance {
            collector: provider.collector_version(),
            started_at: snapshot.taken_at.clone(),
            completed_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        };
        let recorded_evidence = recorded.clone();
        let previous = previous
            .filter(|p| p.id != snapshot.id)
            .and_then(|p| reports::load(&dir.join(format!("{}.json", p.id))).ok())
            .map(|r| r.snapshot);
        // A replica's usage counters are its own reads: set them aside before anything compares or judges them.
        history::set_aside_replica_usage(&mut snapshot);
        let same_database = previous.as_ref().filter(|p| history::same_database(p, &snapshot));
        let evictions = trends::evictions(&snapshot);
        if let Some(workload) = snapshot.workload.as_mut().filter(|w| w.is_cumulative()) {
            // A source that can't say when it started counting (pg_stat_statements before 14) gets its window from the
            // scan before.
            let before = same_database.and_then(|p| Some((p.workload.as_ref()?, p.taken_at.as_str())));
            trends::infer_window_referenced(
                workload,
                before,
                &snapshot.taken_at,
                same_database.map(culprit_core::ScanReference::of),
            );
            let history = reports::counters_history(&dir, &snapshot.taken_at);
            if workload.timeline.is_none() {
                trends::from_scans(workload, &snapshot.server.database, &snapshot.taken_at, evictions, &history);
            }
            // Statements whose source doesn't say when they last ran get it from the scans that saw them grow.
            trends::last_seen_from_scans(workload, &snapshot.server.database, &snapshot.taken_at, &history);
        }
        // This scan's changes come before the checks: an index the scans saw appear this week isn't unused yet, nor
        // one whose usage counters started over.
        let changes = history::diff(previous.as_ref(), &snapshot, recorded);
        let age_inputs = history::date_new_indexes_traced(&dir, &mut snapshot, &changes);
        // Lock escalations, index contention and operational counters are counted since startup with no time for each:
        // compare against the earliest compatible scan covering the selected window.
        reports::apply_counter_baselines(&dir, &mut snapshot, previous.as_ref());
        // pg_stat_statements' evictions are counted since its last reset: the oldest of Culprit's scans a day or more
        // back tells the rate they go at now, else the previous scan's count tells new ones.
        snapshot.evictions_before = reports::evictions_baseline(&dir, &snapshot).or_else(|| {
            previous
                .as_ref()
                .filter(|p| history::same_database(p, &snapshot))
                .and_then(|p| culprit_core::CountBaseline::from_setting(p, &snapshot, trends::EVICTIONS_SETTING))
        });
        // How big each table was a day or more ago tells how fast it grows.
        snapshot.sizes_before = history::growth_baseline(&dir, &snapshot);
        let report = analysis::collected(snapshot, collected, collection, recorded_evidence, age_inputs, self.checks);
        reports::save(&dir, &report)?;
        history::keep_scan(&dir, &report.snapshot, previous.as_ref(), &changes);
        Ok(report)
    }
}
