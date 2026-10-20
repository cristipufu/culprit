//! The identity and clock behind a measurement. Comparisons ask this contract before subtracting values.
//! Older reports retain an explicit legacy comparison; a known endpoint is never filled in from today's connection.

use crate::{Engine, Period, Snapshot, Workload, WorkloadKind, format::epoch_seconds};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum DatabaseLocation {
    Server { host: String, port: u16, instance: Option<String> },
    Socket { directory: String, port: u16 },
    File { path: String },
    Unknown,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct DatabaseIdentity {
    pub engine: Engine,
    pub database: String,
    pub location: DatabaseLocation,
    // An engine-provided incarnation, when available. Never inferred from a database name or server start time.
    pub incarnation: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ComparisonReason {
    DifferentEngine,
    DifferentDatabase,
    DifferentEndpoint,
    UnknownIdentity,
    DifferentSource,
    DifferentScope,
    DifferentPeriod,
    Reset,
    InvalidTime,
}

impl ComparisonReason {
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::DifferentEngine => "The scans describe different database engines.",
            Self::DifferentDatabase => "The scans describe different databases or database incarnations.",
            Self::DifferentEndpoint => "The scans describe different server endpoints or database files.",
            Self::UnknownIdentity => "One scan does not record enough database identity to establish a comparison.",
            Self::DifferentSource => "The measurements come from different sources.",
            Self::DifferentScope => "The measurements cover different scopes.",
            Self::DifferentPeriod => "The measurements cover different kinds or lengths of periods.",
            Self::Reset => "The measurement counters started over between these scans.",
            Self::InvalidTime => "The measurements do not establish a valid earlier and later time.",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "reason", rename_all = "kebab-case")]
#[ts(export)]
pub enum Comparison {
    Compatible,
    /// Both inputs predate endpoint identity. Retained for saved-data compatibility, never a claim about the endpoint.
    Legacy,
    Incompatible(ComparisonReason),
}

impl Comparison {
    pub fn allows_comparison(&self) -> bool {
        !matches!(self, Self::Incompatible(_))
    }
    pub fn reason(&self) -> Option<&ComparisonReason> {
        if let Self::Incompatible(reason) = self { Some(reason) } else { None }
    }
}

impl DatabaseIdentity {
    pub fn server(engine: Engine, database: &str, host: &str, port: u16, instance: Option<String>) -> Self {
        Self {
            engine,
            database: database.into(),
            location: if host.trim().starts_with('/') {
                DatabaseLocation::Socket { directory: host.trim().into(), port }
            } else {
                DatabaseLocation::Server {
                    host: host.trim().trim_end_matches('.').to_ascii_lowercase(),
                    port,
                    instance,
                }
            },
            incarnation: None,
        }
    }
    pub fn file(database: &str, path: String) -> Self {
        Self {
            engine: Engine::Sqlite,
            database: database.into(),
            location: DatabaseLocation::File { path },
            incarnation: None,
        }
    }
    pub fn comparison(&self, other: &Self) -> Comparison {
        use ComparisonReason::*;
        let different = |reason| Comparison::Incompatible(reason);
        if self.engine != other.engine {
            return different(DifferentEngine);
        }
        if self.database.trim().is_empty() || other.database.trim().is_empty() {
            return different(UnknownIdentity);
        }
        if matches!((&self.incarnation, &other.incarnation), (Some(a), Some(b)) if a != b) {
            return different(DifferentDatabase);
        }
        if self.incarnation.is_some() != other.incarnation.is_some() {
            return different(UnknownIdentity);
        }
        match (&self.location, &other.location) {
            (DatabaseLocation::Unknown, DatabaseLocation::Unknown) => {
                if self.database.eq_ignore_ascii_case(&other.database) {
                    Comparison::Legacy
                } else {
                    different(DifferentDatabase)
                }
            }
            (DatabaseLocation::Unknown, _) | (_, DatabaseLocation::Unknown) => different(UnknownIdentity),
            (
                DatabaseLocation::Server { host: a, port: ap, instance: ai },
                DatabaseLocation::Server { host: b, port: bp, instance: bi },
            ) => {
                if a.trim().is_empty() || b.trim().is_empty() || *ap == 0 || *bp == 0 {
                    return different(UnknownIdentity);
                }
                if !a.trim().trim_end_matches('.').eq_ignore_ascii_case(b.trim().trim_end_matches('.'))
                    || ap != bp
                    || ai != bi
                {
                    return different(DifferentEndpoint);
                }
                if self.database != other.database {
                    return different(DifferentDatabase);
                }
                Comparison::Compatible
            }
            (DatabaseLocation::File { path: a }, DatabaseLocation::File { path: b }) => {
                if a.is_empty() || b.is_empty() {
                    different(UnknownIdentity)
                } else if a != b {
                    different(DifferentEndpoint)
                } else {
                    // A symlink can give the same canonical file a different display name.
                    Comparison::Compatible
                }
            }
            (
                DatabaseLocation::Socket { directory: a, port: ap },
                DatabaseLocation::Socket { directory: b, port: bp },
            ) => {
                if a.is_empty() || b.is_empty() || *ap == 0 || *bp == 0 {
                    different(UnknownIdentity)
                } else if a != b || ap != bp {
                    different(DifferentEndpoint)
                } else if self.database != other.database {
                    different(DifferentDatabase)
                } else {
                    Comparison::Compatible
                }
            }
            _ => different(DifferentEndpoint),
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "source", rename_all = "kebab-case")]
#[ts(export)]
pub enum MeasurementSource {
    Workload(WorkloadKind),
    Named(String),
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum MeasurementScope {
    Database,
    Server,
    Session,
    File,
    Unknown,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum MeasurementPeriod {
    Window { span: Option<Period>, hours: Option<f64>, requested_minutes: Option<u32> },
    Cumulative { observed_at: String, hours: Option<f64> },
    ObservedScans { span: Period },
    Sample { span: Period },
    Instant { at: String },
    Unknown,
}

impl MeasurementPeriod {
    pub fn hours(&self) -> Option<f64> {
        let hours = match self {
            Self::Window { span: Some(span), hours, .. } => {
                let actual = span.hours();
                if actual <= 0.0 || hours.is_some_and(|h| !h.is_finite() || ((h - actual) * 3600.0).abs() > 1.0) {
                    return None;
                }
                Some(actual)
            }
            Self::ObservedScans { span } | Self::Sample { span } => Some(span.hours()),
            Self::Window { hours, .. } | Self::Cumulative { hours, .. } => *hours,
            _ => None,
        };
        hours.filter(|h| h.is_finite() && *h > 0.0)
    }
    pub fn is_cumulative(&self) -> bool {
        matches!(self, Self::Cumulative { .. })
    }
    pub fn is_windowed(&self) -> bool {
        matches!(self, Self::Window { .. } | Self::ObservedScans { .. } | Self::Sample { .. })
    }
    pub fn observed_at(&self) -> Option<&str> {
        match self {
            Self::Cumulative { observed_at, .. } => Some(observed_at),
            Self::Window { span: Some(span), .. } | Self::ObservedScans { span } | Self::Sample { span } => {
                Some(&span.end)
            }
            Self::Instant { at } => Some(at),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
#[ts(export)]
pub enum ResetMarker {
    At(String),
    Generation(String),
    Unknown,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct MeasurementContext {
    pub database: DatabaseIdentity,
    pub source: MeasurementSource,
    pub scope: MeasurementScope,
    pub period: MeasurementPeriod,
    pub reset: ResetMarker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CounterTransition {
    Continued,
    Reset,
    Unmarked,
    Incompatible(ComparisonReason),
}

impl MeasurementContext {
    pub fn workload(
        identity: DatabaseIdentity,
        workload: &Workload,
        taken_at: &str,
        requested_minutes: Option<u32>,
    ) -> Self {
        let source = workload
            .kind()
            .map(MeasurementSource::Workload)
            .unwrap_or_else(|| MeasurementSource::Named(workload.source.clone()));
        let hours = Some(workload.window_hours).filter(|h| h.is_finite() && *h > 0.0);
        let bounds = workload
            .window_start
            .as_ref()
            .zip(workload.window_end.as_ref())
            .map(|(start, end)| Period { start: start.clone(), end: end.clone() });
        let invalid_bounds = bounds.as_ref().is_some_and(|p| p.hours() <= 0.0);
        let hours = hours.filter(|_| !invalid_bounds);
        let span = bounds.filter(|p| hours.is_some() && p.hours() > 0.0);
        let period = if workload.windowed {
            MeasurementPeriod::Window {
                span,
                hours,
                requested_minutes: if matches!(workload.kind(), Some(WorkloadKind::QueryLog)) {
                    None
                } else {
                    requested_minutes
                },
            }
        } else {
            MeasurementPeriod::Cumulative {
                // A cache's window_end can be the last execution, even on a later scan with no new calls.
                // Counter differences are bounded by collection times, not by the most recent execution.
                observed_at: taken_at.into(),
                hours,
            }
        };
        let reset = if workload.windowed || !matches!(workload.kind(), Some(WorkloadKind::StatementStats)) {
            // The oldest cached plan/digest is not a source-wide reset marker.
            ResetMarker::Unknown
        } else {
            workload.window_start.clone().map(ResetMarker::At).unwrap_or(ResetMarker::Unknown)
        };
        Self { database: identity, source, scope: MeasurementScope::Database, period, reset }
    }

    pub fn series_comparison(&self, other: &Self) -> Comparison {
        use ComparisonReason::*;
        let identity = self.database.comparison(&other.database);
        if !identity.allows_comparison() {
            return identity;
        }
        if self.source != other.source {
            return Comparison::Incompatible(DifferentSource);
        }
        if self.scope != other.scope || self.scope == MeasurementScope::Unknown {
            return Comparison::Incompatible(DifferentScope);
        }
        identity
    }

    /// A counter reset is an engine fact, never inferred merely because a number shrank.
    pub fn counter_transition(&self, later: &Self) -> CounterTransition {
        if let Comparison::Incompatible(reason) = self.series_comparison(later) {
            return CounterTransition::Incompatible(reason);
        }
        if !self.period.is_cumulative() || !later.period.is_cumulative() {
            return CounterTransition::Incompatible(ComparisonReason::DifferentPeriod);
        }
        let times =
            self.period.observed_at().and_then(epoch_seconds).zip(later.period.observed_at().and_then(epoch_seconds));
        let Some((before, after)) = times.filter(|(a, b)| b > a) else {
            return CounterTransition::Incompatible(ComparisonReason::InvalidTime);
        };
        for (reset, observed) in [(&self.reset, before), (&later.reset, after)] {
            if let ResetMarker::At(value) = reset
                && epoch_seconds(value).is_none_or(|reset| reset > observed)
            {
                return CounterTransition::Incompatible(ComparisonReason::InvalidTime);
            }
        }
        match (&self.reset, &later.reset) {
            (ResetMarker::At(a), ResetMarker::At(b)) => {
                let Some((a, b)) = epoch_seconds(a).zip(epoch_seconds(b)) else {
                    return CounterTransition::Incompatible(ComparisonReason::InvalidTime);
                };
                if a > before || b > after {
                    return CounterTransition::Incompatible(ComparisonReason::InvalidTime);
                }
                if (b - a).abs() <= 1.0 {
                    CounterTransition::Continued
                } else if b > before {
                    CounterTransition::Reset
                } else {
                    CounterTransition::Incompatible(ComparisonReason::InvalidTime)
                }
            }
            (_, ResetMarker::At(b)) => match epoch_seconds(b) {
                Some(b) if b > before && b <= after => CounterTransition::Reset,
                Some(b) if b <= after => CounterTransition::Unmarked,
                _ => CounterTransition::Incompatible(ComparisonReason::InvalidTime),
            },
            (ResetMarker::Generation(a), ResetMarker::Generation(b)) => {
                if a == b {
                    CounterTransition::Continued
                } else {
                    CounterTransition::Reset
                }
            }
            _ => CounterTransition::Unmarked,
        }
    }

    /// Findings from different selected windows cannot verify each other's absence.
    pub fn report_comparison(&self, other: &Self) -> Comparison {
        let series = self.series_comparison(other);
        if !series.allows_comparison() {
            return series;
        }
        let same_period = match (&self.period, &other.period) {
            (MeasurementPeriod::Cumulative { .. }, MeasurementPeriod::Cumulative { .. }) => {
                match self.counter_transition(other) {
                    CounterTransition::Reset => return Comparison::Incompatible(ComparisonReason::Reset),
                    CounterTransition::Incompatible(reason) => return Comparison::Incompatible(reason),
                    _ => true,
                }
            }
            (
                MeasurementPeriod::Window { requested_minutes: a, .. },
                MeasurementPeriod::Window { requested_minutes: b, .. },
            ) => {
                a == b
                    && self.period.hours().zip(other.period.hours()).is_some_and(|(before, after)| {
                        // Query Store includes whole boundary intervals. Two reads of the same selected
                        // window naturally differ by seconds (or an interval boundary) as that window rolls.
                        let complete = a.is_some_and(|minutes| {
                            let requested = f64::from(minutes) / 60.0;
                            before + 1.0 / 3600.0 >= requested && after + 1.0 / 3600.0 >= requested
                        });
                        complete || ((before - after) * 3600.0).abs() <= 1.0
                    })
            }
            _ => false,
        };
        if same_period { series } else { Comparison::Incompatible(ComparisonReason::DifferentPeriod) }
    }
}

impl Snapshot {
    /// Fill only missing context for an identified scan. A saved legacy scan never acquires today's endpoint.
    pub fn ensure_measurement(&mut self) {
        if self.identity.is_some() && self.workload.as_ref().is_some_and(|w| w.measurement.is_none()) {
            let context = self.workload_measurement();
            if let Some(workload) = &mut self.workload {
                workload.measurement = context;
            }
        }
    }
    pub fn database_identity(&self) -> DatabaseIdentity {
        self.identity.clone().unwrap_or_else(|| DatabaseIdentity {
            engine: self.server.engine,
            database: self.server.database.clone(),
            location: DatabaseLocation::Unknown,
            incarnation: None,
        })
    }
    pub fn database_comparison(&self, other: &Self) -> Comparison {
        if [&self, &other].iter().any(|s| {
            s.identity.as_ref().is_some_and(|i| i.engine != s.server.engine || i.database != s.server.database)
        }) {
            return Comparison::Incompatible(ComparisonReason::UnknownIdentity);
        }
        self.database_identity().comparison(&other.database_identity())
    }
    pub fn same_database(&self, other: &Self) -> bool {
        self.database_comparison(other).allows_comparison()
    }
    pub fn measurement_consistency(&self) -> Comparison {
        if !self.database_comparison(self).allows_comparison() {
            return Comparison::Incompatible(ComparisonReason::UnknownIdentity);
        }
        let Some(context) = self.workload_measurement() else { return self.database_comparison(self) };
        if context.scope != MeasurementScope::Database {
            return Comparison::Incompatible(ComparisonReason::DifferentScope);
        }
        self.database_identity().comparison(&context.database)
    }
    /// Context actually captured with an identified scan, as opposed to a legacy interpretation.
    pub fn captured_measurement(&self) -> Option<MeasurementContext> {
        if self.identity.is_some() || self.workload.as_ref().is_some_and(|w| w.measurement.is_some()) {
            self.workload_measurement()
        } else {
            None
        }
    }
    pub fn report_comparison(&self, later: &Self) -> Comparison {
        for snapshot in [self, later] {
            let consistency = snapshot.measurement_consistency();
            if !consistency.allows_comparison() {
                return consistency;
            }
        }
        let identity = self.database_comparison(later);
        if !identity.allows_comparison() {
            return identity;
        }
        match (self.workload_measurement(), later.workload_measurement()) {
            (Some(a), Some(b)) if self.captured_measurement().is_none() && later.captured_measurement().is_none() => {
                // Older exports did not establish that their dates delimit the reported hours. Keep their
                // original clock semantics, while still refusing different sources and selected window lengths.
                let series = a.series_comparison(&b);
                if !series.allows_comparison() {
                    return series;
                }
                let before = self.workload.as_ref().expect("measurement has a workload");
                let after = later.workload.as_ref().expect("measurement has a workload");
                if before.windowed != after.windowed
                    || (before.windowed && ((before.window_hours - after.window_hours) * 3600.0).abs() > 1.0)
                {
                    Comparison::Incompatible(ComparisonReason::DifferentPeriod)
                } else {
                    series
                }
            }
            (Some(a), Some(b)) => a.report_comparison(&b),
            (None, None) => identity,
            _ => Comparison::Incompatible(ComparisonReason::DifferentSource),
        }
    }
    pub fn workload_measurement(&self) -> Option<MeasurementContext> {
        let workload = self.workload.as_ref()?;
        Some(workload.measurement.clone().unwrap_or_else(|| {
            MeasurementContext::workload(
                self.database_identity(),
                workload,
                &self.taken_at,
                self.options.as_ref().map(|o| o.window_in_minutes()),
            )
        }))
    }
    pub fn bind_identity(&mut self, identity: DatabaseIdentity) {
        self.identity = Some(identity);
        let context = self.workload.as_ref().map(|w| {
            MeasurementContext::workload(
                self.database_identity(),
                w,
                &self.taken_at,
                self.options.as_ref().map(|o| o.window_in_minutes()),
            )
        });
        if let Some(w) = self.workload.as_mut() {
            w.measurement = context;
        }
    }
}

impl Workload {
    pub fn is_cumulative(&self) -> bool {
        self.measurement.as_ref().map(|c| c.period.is_cumulative()).unwrap_or(!self.windowed)
    }
    pub fn is_windowed(&self) -> bool {
        self.measurement.as_ref().map(|c| c.period.is_windowed()).unwrap_or(self.windowed)
    }
    pub fn measurement_hours(&self) -> f64 {
        self.measurement.as_ref().map(|c| c.period.hours().unwrap_or(0.0)).unwrap_or_else(|| {
            if self.window_hours.is_finite() && self.window_hours >= 0.0 { self.window_hours } else { 0.0 }
        })
    }
}

impl crate::Digest {
    pub fn database_comparison(&self, later: &Snapshot) -> Comparison {
        if self.identity.as_ref().is_some_and(|i| i.database != self.database)
            || !later.database_comparison(later).allows_comparison()
        {
            return Comparison::Incompatible(ComparisonReason::UnknownIdentity);
        }
        match (&self.identity, &later.identity) {
            (Some(a), Some(b)) => a.comparison(b),
            (None, None) if self.database.eq_ignore_ascii_case(&later.server.database) => Comparison::Legacy,
            (None, None) => Comparison::Incompatible(ComparisonReason::DifferentDatabase),
            _ => Comparison::Incompatible(ComparisonReason::UnknownIdentity),
        }
    }
    pub fn comparison(&self, later: &Snapshot) -> Comparison {
        let identity = self.database_comparison(later);
        if !identity.allows_comparison() {
            return identity;
        }
        match (&self.measurement, later.captured_measurement()) {
            (Some(a), Some(b)) => {
                if self.identity.as_ref().is_some_and(|id| !id.comparison(&a.database).allows_comparison())
                    || a.database.database != self.database
                    || !later.measurement_consistency().allows_comparison()
                {
                    Comparison::Incompatible(ComparisonReason::UnknownIdentity)
                } else {
                    a.report_comparison(&b)
                }
            }
            (None, None) => identity,
            _ => Comparison::Incompatible(ComparisonReason::DifferentSource),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database(host: &str) -> DatabaseIdentity {
        DatabaseIdentity::server(Engine::Postgres, "shop", host, 5432, None)
    }
    fn context(at: &str, reset: ResetMarker) -> MeasurementContext {
        MeasurementContext {
            database: database("db.example"),
            source: MeasurementSource::Workload(WorkloadKind::StatementStats),
            scope: MeasurementScope::Database,
            period: MeasurementPeriod::Cumulative { observed_at: at.into(), hours: Some(24.0) },
            reset,
        }
    }
    #[test]
    fn identity_includes_endpoint_port_file_and_incarnation() {
        let a = database("DB.EXAMPLE.");
        assert_eq!(a.comparison(&database("db.example")), Comparison::Compatible);
        assert_eq!(
            a.comparison(&database("other.example")),
            Comparison::Incompatible(ComparisonReason::DifferentEndpoint)
        );
        let mut b = database("db.example");
        if let DatabaseLocation::Server { port, .. } = &mut b.location {
            *port = 55432;
        }
        assert!(!a.comparison(&b).allows_comparison());
        let mut b = a.clone();
        b.database = "Shop".into();
        assert!(!a.comparison(&b).allows_comparison(), "observed database identifiers retain their case");
        let mut a = a;
        a.incarnation = Some("first".into());
        let mut b = a.clone();
        b.incarnation = Some("replacement".into());
        assert!(!a.comparison(&b).allows_comparison());
        let a = DatabaseIdentity::file("shop", "/apps/one/shop.sqlite".into());
        let b = DatabaseIdentity::file("shop", "/apps/two/shop.sqlite".into());
        assert!(!a.comparison(&b).allows_comparison());
    }
    #[test]
    fn socket_paths_are_not_normalized_as_dns_names() {
        let a = DatabaseIdentity::server(Engine::Postgres, "shop", "/tmp/Socket", 5432, None);
        let b = DatabaseIdentity::server(Engine::Postgres, "shop", "/tmp/socket", 5432, None);
        assert!(!a.comparison(&b).allows_comparison());
    }
    #[test]
    fn legacy_identity_is_explicit_and_not_borrowed_from_a_new_endpoint() {
        let mut a = database("db.example");
        a.location = DatabaseLocation::Unknown;
        let mut b = a.clone();
        b.database = "SHOP".into();
        assert_eq!(a.comparison(&b), Comparison::Legacy);
        assert_eq!(a.comparison(&database("db.example")), Comparison::Incompatible(ComparisonReason::UnknownIdentity));
        let mut invalid = database("");
        invalid.database.clear();
        assert!(!invalid.comparison(&invalid).allows_comparison());
    }
    #[test]
    fn counter_comparison_requires_matching_source_scope_and_clock_order() {
        let a = context("2026-10-10T00:00:00Z", ResetMarker::Unknown);
        let b = context("2026-10-11T00:00:00Z", ResetMarker::Unknown);
        assert_eq!(a.counter_transition(&b), CounterTransition::Unmarked);
        assert!(matches!(b.counter_transition(&a), CounterTransition::Incompatible(ComparisonReason::InvalidTime)));
        let mut wrong = b.clone();
        wrong.scope = MeasurementScope::Server;
        assert!(matches!(
            a.counter_transition(&wrong),
            CounterTransition::Incompatible(ComparisonReason::DifferentScope)
        ));
        wrong = b.clone();
        wrong.source = MeasurementSource::Workload(WorkloadKind::PlanCache);
        assert!(matches!(
            a.counter_transition(&wrong),
            CounterTransition::Incompatible(ComparisonReason::DifferentSource)
        ));
        wrong = b.clone();
        wrong.database = database("elsewhere.example");
        assert!(matches!(
            a.counter_transition(&wrong),
            CounterTransition::Incompatible(ComparisonReason::DifferentEndpoint)
        ));
    }
    #[test]
    fn reset_marker_equivalent_timezones_and_confirmed_resets_have_distinct_results() {
        let a = context("2026-10-10T00:00:00Z", ResetMarker::At("2026-10-01T00:00:00Z".into()));
        let mut b = context("2026-10-11T00:00:00Z", ResetMarker::At("2026-10-01T03:00:00+03:00".into()));
        assert_eq!(a.counter_transition(&b), CounterTransition::Continued);
        b.reset = ResetMarker::At("2026-10-10T12:00:00Z".into());
        assert_eq!(a.counter_transition(&b), CounterTransition::Reset);
        assert_eq!(a.report_comparison(&b), Comparison::Incompatible(ComparisonReason::Reset));
        b.reset = ResetMarker::At("2026-10-09T12:00:00Z".into());
        assert!(matches!(a.counter_transition(&b), CounterTransition::Incompatible(ComparisonReason::InvalidTime)));
        b.reset = ResetMarker::At("bad timestamp".into());
        assert!(matches!(a.counter_transition(&b), CounterTransition::Incompatible(ComparisonReason::InvalidTime)));
    }
    #[test]
    fn interval_measurements_are_never_subtracted_as_running_totals() {
        let a = context("2026-10-10T00:00:00Z", ResetMarker::Unknown);
        let mut b = context("2026-10-11T00:00:00Z", ResetMarker::Unknown);
        b.period = MeasurementPeriod::ObservedScans {
            span: Period { start: "2026-10-10T00:00:00Z".into(), end: "2026-10-11T00:00:00Z".into() },
        };
        assert_eq!(b.period.hours(), Some(24.0));
        assert!(matches!(a.counter_transition(&b), CounterTransition::Incompatible(ComparisonReason::DifferentPeriod)));
    }
    #[test]
    fn selected_window_mismatches_cannot_verify_a_fix() {
        let mut a = context("2026-10-10T00:00:00Z", ResetMarker::Unknown);
        a.period = MeasurementPeriod::Window {
            span: Some(Period { start: "2026-10-09T00:00:00Z".into(), end: "2026-10-10T00:00:00Z".into() }),
            hours: Some(24.0),
            requested_minutes: Some(1440),
        };
        let mut b = a.clone();
        assert_eq!(a.report_comparison(&b), Comparison::Compatible);
        if let MeasurementPeriod::Window { requested_minutes, .. } = &mut b.period {
            *requested_minutes = Some(60);
        }
        assert_eq!(a.report_comparison(&b), Comparison::Incompatible(ComparisonReason::DifferentPeriod));
        b = a.clone();
        if let MeasurementPeriod::Window { hours, .. } = &mut b.period {
            *hours = Some(1.0);
        }
        assert!(b.period.hours().is_none(), "contradictory clocks are not silently reconciled");
    }
    #[test]
    fn old_reports_round_trip_without_inventing_context_or_identity() {
        let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        assert!(s.identity.is_none());
        assert!(s.workload.as_ref().unwrap().measurement.is_none());
        let saved = serde_json::to_value(&s).unwrap();
        assert!(saved.get("identity").is_none());
        assert!(saved["workload"].get("measurement").is_none());
        assert_eq!(s.database_comparison(&s), Comparison::Legacy);
        let mut identified = s.clone();
        identified.bind_identity(DatabaseIdentity::server(
            s.server.engine,
            &s.server.database,
            "db.example",
            5432,
            None,
        ));
        let restored: Snapshot = serde_json::from_value(serde_json::to_value(&identified).unwrap()).unwrap();
        assert_eq!(restored.identity, identified.identity);
        assert_eq!(restored.workload.as_ref().unwrap().measurement, identified.workload.as_ref().unwrap().measurement);
        assert!(!identified.same_database(&s));
    }
    #[test]
    fn typed_period_is_canonical_and_cache_age_is_not_a_reset() {
        let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        let mut w = s.workload.unwrap();
        w.kind = Some(WorkloadKind::PerformanceSchema);
        let c = MeasurementContext::workload(database("db.example"), &w, &s.taken_at, Some(60));
        assert_eq!(c.reset, ResetMarker::Unknown);
        w.measurement = Some(c);
        w.windowed = true;
        w.window_hours = 999.0;
        assert!(w.is_cumulative());
        assert_ne!(w.measurement_hours(), 999.0);
    }
    #[test]
    fn cumulative_collection_time_is_not_the_last_execution_time() {
        let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        let mut w = s.workload.unwrap();
        w.kind = Some(WorkloadKind::PlanCache);
        w.window_end = Some("2026-10-01T00:00:00Z".into());
        let before = MeasurementContext::workload(database("db.example"), &w, "2026-10-10T00:00:00Z", None);
        let after = MeasurementContext::workload(database("db.example"), &w, "2026-10-11T00:00:00Z", None);
        assert_eq!(before.counter_transition(&after), CounterTransition::Unmarked);
        assert_eq!(after.period.observed_at(), Some("2026-10-11T00:00:00Z"));
    }
    #[test]
    fn rolling_selected_windows_compare_when_both_cover_the_request() {
        let mut a = context("2026-10-10T00:00:00Z", ResetMarker::Unknown);
        a.period = MeasurementPeriod::Window { span: None, hours: Some(24.2), requested_minutes: Some(1440) };
        let mut b = a.clone();
        b.period = MeasurementPeriod::Window { span: None, hours: Some(24.8), requested_minutes: Some(1440) };
        assert!(a.report_comparison(&b).allows_comparison());
        b.period = MeasurementPeriod::Window { span: None, hours: Some(2.0), requested_minutes: Some(1440) };
        assert!(!a.report_comparison(&b).allows_comparison(), "partial coverage cannot verify disappearance");
        b.period = MeasurementPeriod::Window { span: None, hours: Some(24.8), requested_minutes: Some(60) };
        assert!(!a.report_comparison(&b).allows_comparison(), "different requests still do not compare");
    }
    #[test]
    fn an_unknown_selected_window_never_claims_one_minute_or_a_daily_rate() {
        let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        let mut w = s.workload.unwrap();
        let mut c = context("2026-10-10T00:00:00Z", ResetMarker::Unknown);
        c.period = MeasurementPeriod::Window { span: None, hours: None, requested_minutes: Some(60) };
        w.measurement = Some(c);
        assert_eq!(w.window_phrase(), "the recorded workload window");
        assert!(w.rate_per_day(1000.0).is_none());
    }
    #[test]
    fn untimed_log_records_never_acquire_a_period_from_other_records_dates() {
        let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        let mut w = s.workload.unwrap();
        w.kind = Some(WorkloadKind::QueryLog);
        w.log = Some("JSON".into());
        w.windowed = true;
        w.window_hours = 0.0;
        w.window_start = Some("2026-10-10T00:00:00Z".into());
        w.window_end = Some("2026-10-11T00:00:00Z".into());
        let c = MeasurementContext::workload(database("db.example"), &w, &s.taken_at, Some(60));
        assert!(c.period.hours().is_none());
        if let MeasurementPeriod::Window { requested_minutes, .. } = c.period {
            assert_eq!(requested_minutes, None);
        } else {
            panic!("log span");
        }
    }
}
