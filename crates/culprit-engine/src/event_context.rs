//! Historical and cloud context for a connection: query comparisons, marks, schema events and throttling.
//! Connection lookup/provider resolution and saved-report analysis are separate injected dependencies.

use crate::{
    ConnectionInfo, EventLog, EventsRequest, changes, cloud, connection::Connections, history, marks,
    reporting::Reports, reports, trends,
};
use culprit_core::{Engine, Error, ErrorCode, Result};

pub(crate) struct Events<'a> {
    pub connections: Connections<'a>,
    pub reports: Reports<'a>,
}

impl Events<'_> {
    /// What changed between two periods: each query's runs and time per run before and after, judged with the same
    /// rules everywhere. Query Store answers from its own intervals (any period it still keeps); sources that only keep
    /// running totals answer from Culprit's saved scans, so periods snap to the scans nearest to what was asked.
    pub(crate) async fn changes(&self, name: &str, request: &changes::ChangeRequest) -> Result<culprit_core::Changes> {
        let info = self.connections.connection(name)?;
        let latest = self.reports.latest_report(name, None)?;
        let options = changes::options(request);
        let now = chrono::Utc::now().timestamp() as f64;
        let workload = latest.as_ref().and_then(|r| r.snapshot.workload.as_ref());
        // A log Culprit read is one period, and nothing counts between scans of it.
        if let Some(log) = workload.and_then(|w| w.log.as_deref()) {
            return Err(Error::new(
                ErrorCode::Unsupported,
                format!("{} keeps no statement history to compare two periods of", info.engine.label()),
            )
            .with_hint(format!(
                "The {log} log Culprit read is one period: scan again with a log written after the change, and compare the two reports' queries."
            )));
        }
        // A database with no statement history of its own (SQLite) has no two periods to compare.
        if !info.engine.abilities().periods {
            return Err(info.engine.refused(culprit_core::Ability::Periods));
        }
        let windowed = workload.map(|w| w.is_windowed()).unwrap_or(info.engine == Engine::Mssql);
        if windowed {
            let since = workload.and_then(|w| w.window_start.as_deref()).and_then(culprit_core::changes::epoch);
            let (baseline, recent) = changes::resolve(request, (since.unwrap_or(now - 86_400.0), now))?;
            let provider = self.connections.provider(name).await?;
            if let Some(data) =
                self.connections.forget_rejected(name, provider.period_stats(&baseline, &recent).await)?
            {
                let mut out = culprit_core::changes::compare(name, info.engine, data, &options);
                changes::describe_switches(&mut out, provider.as_ref(), 5).await;
                // Query Store periods run up to now; what the database dated since the last scan isn't saved yet.
                let last_scan =
                    latest.as_ref().map(|r| r.snapshot.taken_at.clone()).unwrap_or_else(|| recent.start.clone());
                let live = provider.recorded_changes(&last_scan).await.unwrap_or_default();
                self.add_context(&info, &mut out, request.cloud, live).await;
                return Ok(out);
            }
        }
        // Running totals only: the difference between two scans is what ran in between.
        let latest_counters = latest.as_ref().and_then(trends::Counters::of);
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let history: Vec<trends::Counters> = reports::counters_history(&self.reports.paths.reports_dir(name), &at)
            .into_iter()
            .filter(|c| latest_counters.as_ref().is_none_or(|l| l.same_series(c)))
            .collect();
        let times: Vec<f64> = history.iter().filter_map(|c| culprit_core::changes::epoch(&c.taken_at)).collect();
        let first = times.iter().copied().fold(f64::INFINITY, f64::min);
        let last = times.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let (first, last) = if first.is_finite() && last > first { (first, last) } else { (now - 86_400.0, now) };
        let defaults = request.is_default().then(|| changes::default_scan_periods(&history)).flatten();
        let (baseline, recent) = match defaults {
            Some(periods) => periods,
            None => changes::resolve(request, (first, last))?,
        };
        let data = changes::from_scans(&history, &baseline, &recent);
        let mut out = culprit_core::changes::compare(name, info.engine, data, &options);
        self.add_context(&info, &mut out, request.cloud, Vec::new()).await;
        Ok(out)
    }

    /// Line the comparison up with what changed around it: marks, schema and settings changes, restarts, throttling.
    async fn add_context(
        &self,
        info: &ConnectionInfo,
        out: &mut culprit_core::Changes,
        cloud: Option<bool>,
        live: Vec<culprit_core::ChangeEvent>,
    ) {
        let (Some(start), Some(end)) =
            (culprit_core::changes::epoch(&out.baseline.start), culprit_core::changes::epoch(&out.recent.end))
        else {
            return;
        };
        let mut notes = Vec::new();
        let mut events = self.events_in(info, start, end, cloud, &mut notes).await;
        events.extend(live);
        out.notes.extend(notes);
        culprit_core::events::correlate(out, events);
    }

    /// Marks, what Culprit's scans noticed and, for cloud databases, throttling episodes between `start` and `end`.
    async fn events_in(
        &self,
        info: &ConnectionInfo,
        start: f64,
        end: f64,
        cloud: Option<bool>,
        notes: &mut Vec<String>,
    ) -> Vec<culprit_core::ChangeEvent> {
        let dir = self.reports.paths.reports_dir(&info.name);
        let mut events: Vec<culprit_core::ChangeEvent> = marks::list(&dir).iter().map(marks::Mark::event).collect();
        events.extend(history::events(&dir, start, end));
        if cloud != Some(false) && cloud::target(info).is_some() {
            match cloud::signals(&dir, info, start, end, false).await {
                Ok(signals) => events.extend(signals.events()),
                Err(e) => notes.push(match e.hint {
                    Some(hint) => format!("Cloud metrics unavailable: {} ({hint}).", e.message.trim_end_matches('.')),
                    None => format!("Cloud metrics unavailable: {}.", e.message.trim_end_matches('.')),
                }),
            }
        }
        culprit_core::events::in_range(events, start, end)
    }

    /// What changed around a connection over a period, oldest first: marks, schema, index and settings changes,
    /// restarts, upgrades and, for cloud databases, throttling. Reads saved scans and the cloud CLI; never the database.
    pub(crate) async fn events(&self, name: &str, request: &EventsRequest) -> Result<EventLog> {
        let info = self.connections.connection(name)?;
        let (start, end) = request.range()?;
        let mut notes = Vec::new();
        let events = self.events_in(&info, start, end, request.cloud, &mut notes).await;
        Ok(EventLog { connection: info.name, start: changes::iso(start), end: changes::iso(end), events, notes })
    }

    /// Put a moment on a connection's timeline (a deploy, a migration) so charts and What changed line queries up with it.
    pub(crate) fn mark(&self, name: &str, text: &str, at: Option<&str>, by: &str) -> Result<marks::Mark> {
        self.connections.connection(name)?;
        marks::add(&self.reports.paths.reports_dir(name), text, at, by)
    }

    pub(crate) fn marks(&self, name: &str) -> Result<Vec<marks::Mark>> {
        self.connections.connection(name)?;
        Ok(marks::list(&self.reports.paths.reports_dir(name)))
    }

    pub(crate) fn unmark(&self, name: &str, id: &str) -> Result<marks::Mark> {
        self.connections.connection(name)?;
        marks::remove(&self.reports.paths.reports_dir(name), id)
    }

    /// What the cloud measured for a connection's server (CloudWatch, Azure Monitor) and when it held the server back,
    /// through the person's own `aws` / `az` sign-in. Cached for a few minutes.
    pub(crate) async fn cloud(&self, name: &str, request: &EventsRequest) -> Result<culprit_core::CloudSignals> {
        let info = self.connections.connection(name)?;
        let (start, end) = request.range()?;
        cloud::signals(&self.reports.paths.reports_dir(name), &info, start, end, request.refresh).await
    }
}
