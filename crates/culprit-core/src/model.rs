use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::plan::Plan;
use crate::provider::SnapshotOptions;

pub const SNAPSHOT_FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Engine {
    Mssql,
    Postgres,
    Mysql,
    /// A SQLite database file: no server, no statement statistics. Culprit reads its schema, settings and plans.
    Sqlite,
}

/// A unit enum's name as it serializes (`query-store`, `engine-built`): serde's own, so a label table can't drift from it.
pub fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

impl Engine {
    /// Every engine, for tables kept per engine (see [`Engine::abilities`]).
    pub const ALL: [Engine; 4] = [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite];

    /// A server Culprit talks to: sessions, waits, deadlocks and server counters exist. A file the application opens
    /// itself (SQLite) has none of these.
    pub fn has_server(self) -> bool {
        self.abilities().sessions
    }
}

/// What an engine can answer (see [`Engine::abilities`]).
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct EngineAbilities {
    /// Sessions, blocking and the Live view.
    pub sessions: bool,
    /// Sampling what the server waits on right now.
    pub wait_sample: bool,
    /// Deadlocks it records (graphs or a counter).
    pub deadlocks: bool,
    /// Running a statement once and reporting what each step actually did (rows, timings).
    pub measures_runs: bool,
    /// Row and cost estimates in its plans.
    pub plan_estimates: bool,
    /// Statements that change data, from the editor.
    pub writes: bool,
    /// Two periods of statement history to compare (What changed).
    pub periods: bool,
    /// The actual plan of a statement's most recent run, kept by the engine (SQL Server 2019+).
    pub last_actual_plan: bool,
    /// Live per-step counters of a running statement, sent every time (SQL Server); elsewhere the plan only when asked.
    pub operator_progress: bool,
    /// A sign-in: a password, a token. A file opened from disk takes none.
    pub sign_in: bool,
}

/// One of an engine's [`EngineAbilities`], to ask about it or for the reason it lacks it ([`crate::EngineSpec::refusal`]).
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Ability {
    Sessions,
    WaitSample,
    Deadlocks,
    MeasuresRuns,
    PlanEstimates,
    Writes,
    Periods,
    LastActualPlan,
    OperatorProgress,
    SignIn,
}

impl Ability {
    pub const ALL: [Ability; 10] = [
        Ability::Sessions,
        Ability::WaitSample,
        Ability::Deadlocks,
        Ability::MeasuresRuns,
        Ability::PlanEstimates,
        Ability::Writes,
        Ability::Periods,
        Ability::LastActualPlan,
        Ability::OperatorProgress,
        Ability::SignIn,
    ];
}

impl EngineAbilities {
    pub fn has(&self, ability: Ability) -> bool {
        match ability {
            Ability::Sessions => self.sessions,
            Ability::WaitSample => self.wait_sample,
            Ability::Deadlocks => self.deadlocks,
            Ability::MeasuresRuns => self.measures_runs,
            Ability::PlanEstimates => self.plan_estimates,
            Ability::Writes => self.writes,
            Ability::Periods => self.periods,
            Ability::LastActualPlan => self.last_actual_plan,
            Ability::OperatorProgress => self.operator_progress,
            Ability::SignIn => self.sign_in,
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Environment {
    Production,
    Staging,
    #[default]
    Development,
    Local,
}

impl Environment {
    pub fn is_production(self) -> bool {
        self == Environment::Production
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum Capability {
    QueryStore,
    PlanCache,
    PgStatStatements,
    PerformanceSchema,
    QueryPlans,
    GenericPlans,
    TableStats,
    IndexUsage,
    MissingIndexes,
    Statistics,
    WaitStats,
    ResourceUsage,
    Sessions,
    Showplan,
    SlowLog,
    /// The statements and their times from a log the application's framework writes, for an engine that keeps none.
    QueryLog,
}

impl Capability {
    /// The label in `engine`'s own words: estimated plans are SHOWPLAN on SQL Server and EXPLAIN elsewhere.
    pub fn label_for(self, engine: Engine) -> &'static str {
        match (self, engine) {
            (Capability::Showplan, Engine::Postgres | Engine::Mysql) => "Estimated plans (EXPLAIN)",
            (Capability::Showplan, Engine::Sqlite) => "Estimated plans (EXPLAIN QUERY PLAN)",
            _ => self.label(),
        }
    }

    /// Its stable id, as it serializes (`query-store`).
    pub fn id(self) -> String {
        wire_name(&self)
    }

    /// A source of statement statistics: without it a scan has no workload, and its reason says why.
    pub fn is_workload_source(self) -> bool {
        matches!(
            self,
            Capability::QueryStore
                | Capability::PlanCache
                | Capability::PgStatStatements
                | Capability::PerformanceSchema
                | Capability::SlowLog
                | Capability::QueryLog
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::QueryStore => "Query Store",
            Capability::PlanCache => "Plan cache statistics",
            Capability::PgStatStatements => "pg_stat_statements",
            Capability::PerformanceSchema => "performance_schema statement digests",
            Capability::QueryPlans => "Query plans",
            Capability::GenericPlans => "Generic plans (PostgreSQL 12+)",
            Capability::TableStats => "Table statistics",
            Capability::IndexUsage => "Index usage statistics",
            Capability::MissingIndexes => "Missing-index suggestions",
            Capability::Statistics => "Optimizer statistics",
            Capability::WaitStats => "Wait statistics",
            Capability::ResourceUsage => "Resource usage",
            Capability::Sessions => "Sessions and blocking",
            Capability::Showplan => "Estimated plans (SHOWPLAN)",
            Capability::SlowLog => "Slow query log table",
            Capability::QueryLog => "Application query log",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct MissingCapability {
    pub capability: Capability,
    pub reason: String,
    pub fix: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ServerInfo {
    pub engine: Engine,
    pub product: String,
    pub version: String,
    pub edition: Option<String>,
    /// The managed service it runs on, as a person names it ("Amazon RDS"). For sentences; [`ServerInfo::platform`]
    /// is what code asks.
    pub cloud: Option<String>,
    /// The managed service it runs on (`None`: a server you run yourself). Older reports have only `cloud`.
    #[serde(default)]
    pub platform: Option<crate::platform::Platform>,
    pub database: String,
    pub database_size_bytes: Option<f64>,
    pub started_at: Option<String>,
    pub read_only_session: bool,
    pub capabilities: Vec<Capability>,
    pub missing: Vec<MissingCapability>,
}

/// Whether a server version string ("5.7.44-log", "10.5.29-MariaDB-ubu2004", "8.0.36") is at least `wanted`. Text
/// that doesn't start with a version counts as new enough, so an unknown build is never refused for its age.
pub fn version_at_least(version: &str, wanted: [u32; 3]) -> bool {
    let numbers: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or_default()
        .split('.')
        .map_while(|p| p.parse().ok())
        .collect();
    if numbers.is_empty() {
        return true;
    }
    let have = [0, 1, 2].map(|i| numbers.get(i).copied().unwrap_or(0));
    have >= wanted
}

impl ServerInfo {
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The managed service it runs on: the one the provider detected, or for an older report the one its label names.
    pub fn platform(&self) -> Option<crate::platform::Platform> {
        self.platform.or_else(|| self.cloud.as_deref().and_then(crate::platform::Platform::from_label))
    }

    /// Whether it runs on `platform`.
    pub fn on(&self, platform: crate::platform::Platform) -> bool {
        self.platform() == Some(platform)
    }

    /// Whether it runs on a managed service at all: no superuser, no configuration file, no operating system.
    pub fn is_managed(&self) -> bool {
        self.platform().is_some() || self.cloud.is_some()
    }

    /// The server's name in sentences: the engine's, except MariaDB, which the MySQL provider also serves.
    pub fn name(&self) -> &'static str {
        if self.is_mariadb() { "MariaDB" } else { self.engine.label() }
    }

    /// MariaDB, which Culprit reads through the MySQL provider: where the two differ (histograms, userstat, ignored
    /// indexes), the rules ask this.
    pub fn is_mariadb(&self) -> bool {
        self.engine == Engine::Mysql && self.product.starts_with("MariaDB")
    }
}

/// What a workload was read from. The rules that depend on it ask this, never the name in `Workload::source`: a slow
/// query log keeps only the runs it logged, performance_schema's histograms count from the server's start, the plan
/// cache forgets what it evicts. A provider sets it; [`Workload::kind`] reads it.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum WorkloadKind {
    /// SQL Server's Query Store: runtime statistics by interval, so a scan picks its window.
    QueryStore,
    /// SQL Server's plan cache: totals since each plan was cached.
    PlanCache,
    /// PostgreSQL's pg_stat_statements: totals since its last reset.
    StatementStats,
    /// MySQL and MariaDB's performance_schema statement digests: totals since the server started or they were reset.
    PerformanceSchema,
    /// MySQL and MariaDB's slow query log table: the runs it logged, one by one.
    SlowLog,
    /// An application's own log of its statements (a Rails or Django log), for an engine that keeps none.
    QueryLog,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Workload {
    /// Where the statements were read from, by name, for sentences ("Query Store covers …").
    pub source: String,
    /// The same, typed: see [`Workload::kind`]. `None` in scans saved before it existed.
    #[serde(default)]
    pub kind: Option<WorkloadKind>,
    /// The source keeps history by time, so a scan can choose its window (Query Store), or records a period of its own
    /// (an application's log): its numbers are of that period, never running totals to subtract. Cumulative sources
    /// (pg_stat_statements, the plan cache) cover "since the last reset" and ignore the requested window.
    #[serde(default)]
    pub windowed: bool,
    /// The application's log the statements were read from, by format ("Rails"), for an engine that keeps no statement
    /// history (SQLite). Its period is what the log's own timestamps span, which may have ended long before the scan;
    /// a log without timestamps covers a period nobody can say. `None` when the database's statistics are the source.
    #[serde(default)]
    pub log: Option<String>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
    pub window_hours: f64,
    pub total_time_ms: f64,
    pub total_cpu_ms: Option<f64>,
    pub total_executions: f64,
    pub query_count: f64,
    pub queries: Vec<QueryStat>,
    /// Database time across the window, oldest first. `None` when the source keeps no history and Culprit has no
    /// earlier scans of this connection to compare with.
    #[serde(default)]
    pub timeline: Option<Timeline>,
    /// Time the source measured but couldn't put on any statement, so it is in neither `queries` nor `total_time_ms`:
    /// MySQL's catch-all digest row once its digest table is full. That row is server-wide (it doesn't say which
    /// database ran it), so shares are of the attributed time and this says how much more there may be. `None` when
    /// nothing was left out or the source can't say.
    #[serde(default)]
    pub untracked_time_ms: Option<f64>,
    /// For a source whose entries count from different moments (the plan cache counts each plan from its compile): the
    /// sum of every entry's time an hour of its own life, in ms. Shares are then weighed by it ([`Workload::share_of`])
    /// so a plan cached for a month doesn't outweigh one compiled an hour ago by its age alone. `None` elsewhere.
    #[serde(default)]
    pub hourly_time_ms: Option<f64>,
    /// Only runs slower than this were recorded (the slow query log's `long_query_time`), so totals, shares and run
    /// counts are of logged slow runs, not of everything the database ran. `None` when every run is counted.
    #[serde(default)]
    pub min_logged_ms: Option<f64>,
}

/// "7 days", "10 hours", "35 minutes": a window's length in words.
pub fn span_words(hours: f64) -> String {
    if hours >= 47.0 {
        return format!("{} days", (hours / 24.0).round());
    }
    if hours >= 1.5 {
        return format!("{} hours", hours.round());
    }
    let minutes = (hours * 60.0).round().max(1.0);
    if minutes == 1.0 { "1 minute".into() } else { format!("{minutes} minutes") }
}

/// Under this many hours of data, a per-day rate would be an extrapolation: say what was measured instead.
pub const PER_DAY_MIN_HOURS: f64 = 20.0;

/// Under this much data a count isn't a rate at all: a threshold on a rate (runs a day, ms a day) needs at least an
/// hour of it, so minutes of counters can't pass for a busy day. Saying "a day" in a sentence needs
/// [`PER_DAY_MIN_HOURS`].
pub const RATE_MIN_HOURS: f64 = 1.0;

impl Workload {
    /// What the workload was read from. Scans saved before providers said so are told by the source's name: the one
    /// place that reads it.
    pub fn kind(&self) -> Option<WorkloadKind> {
        self.kind.or_else(|| match self.source.as_str() {
            "Query Store" => Some(WorkloadKind::QueryStore),
            "Plan cache" => Some(WorkloadKind::PlanCache),
            "pg_stat_statements" => Some(WorkloadKind::StatementStats),
            "performance_schema" => Some(WorkloadKind::PerformanceSchema),
            "slow query log" => Some(WorkloadKind::SlowLog),
            _ if self.log.is_some() => Some(WorkloadKind::QueryLog),
            _ => None,
        })
    }

    /// Hours an entry of this workload has counted: from its first run (a cached plan's compile) to the window's end,
    /// at least a minute. `None` without both times.
    pub fn entry_hours(&self, first_seen: Option<&str>) -> Option<f64> {
        let first = crate::format::epoch_seconds(first_seen?)?;
        let end = crate::format::epoch_seconds(self.window_end.as_deref()?)?;
        Some(((end - first) / 3600.0).max(1.0 / 60.0))
    }

    /// How much of its raw share a statement's time may claim: 1, except in a source whose entries count from different
    /// moments ([`Workload::hourly_time_ms`]), where it is at most its share of the time an hour. A plan cached for a
    /// month then claims what it costs an hour, not a month's total next to an hour's; one compiled recently keeps its
    /// raw share, which can only understate it.
    pub fn share_weight(&self, q: &QueryStat) -> f64 {
        let (Some(hourly), Some(hours)) = (self.hourly_time_ms, self.entry_hours(q.first_seen.as_deref())) else {
            return 1.0;
        };
        if hourly <= 0.0 || self.total_time_ms <= 0.0 {
            return 1.0;
        }
        (self.total_time_ms / (hours * hourly)).min(1.0)
    }

    /// The share of the workload's time `time_ms` of statement `q` is ([`Workload::share`], weighed by
    /// [`Workload::share_weight`]): what every check grades a statement's share on.
    pub fn share_of(&self, time_ms: f64, q: &QueryStat) -> f64 {
        self.share(time_ms) * self.share_weight(q)
    }

    pub fn per_day(&self, value: f64) -> f64 {
        if self.window_hours <= 0.0 {
            return value;
        }
        value * 24.0 / self.window_hours
    }

    /// `value` per day, only when the window is long enough ([`PER_DAY_MIN_HOURS`]) to say so without extrapolating.
    pub fn per_day_measured(&self, value: f64) -> Option<f64> {
        (self.window_hours >= PER_DAY_MIN_HOURS).then(|| self.per_day(value))
    }

    /// `value` as a daily rate for comparing with a threshold, only over at least [`RATE_MIN_HOURS`] of data.
    pub fn rate_per_day(&self, value: f64) -> Option<f64> {
        (self.window_hours >= RATE_MIN_HOURS).then(|| self.per_day(value))
    }

    /// "the last 7 days" for windowed history, "the 35 minutes since statistics were reset" for cumulative counters,
    /// "the 50 minutes the Rails log covers" for an application's log.
    pub fn window_phrase(&self) -> String {
        if let Some(log) = &self.log {
            return if self.window_unknown() {
                format!("the period the {log} log covers")
            } else {
                format!("the {} the {log} log covers", span_words(self.window_hours))
            };
        }
        if self.windowed {
            format!("the last {}", span_words(self.window_hours))
        } else if self.window_unknown() {
            "the time since statistics were last reset".to_string()
        } else {
            format!("the {} since statistics were reset", span_words(self.window_hours))
        }
    }

    /// How often something ran, honestly: "about 4.2K runs a day" over a day or more of data, otherwise the measured
    /// count and its window ("4 runs over the last 10 hours"), and fewer runs than the window has days as what they
    /// are ("1 run in the last 6 days", not "0.2 runs a day"). [`Workload::how_often_of`] says it for one statement.
    pub fn how_often(&self, runs: f64) -> String {
        match self.per_day_measured(runs) {
            Some(per_day) if per_day >= 1.0 => {
                format!("about {} {} a day", crate::format::count(per_day.round()), runs_noun(per_day.round()))
            }
            Some(_) => format!("{} {} in {}", crate::format::count(runs), runs_noun(runs), self.window_phrase()),
            None => format!("{} {} over {}", crate::format::count(runs), runs_noun(runs), self.window_phrase()),
        }
    }

    /// [`Workload::how_often`] for one statement, from when it ran as well as how many times ([`Workload::cadence`]):
    /// "750 runs on Oct 11, 15:51–15:55 UTC" for a burst, never a rate a day it never ran at. On a source of running
    /// totals, with a day or more of Culprit's own scans in which it ran ([`Workload::scans_of`]), the rate is theirs,
    /// what happens now, higher or lower than the totals': "about 1.7K runs a day over 72 hours of Culprit's scans",
    /// with the average since the totals began beside it when the two differ twofold ("about 1.7K runs a day over 72
    /// hours of Culprit's scans (about 388 a day over the 1277 days since statistics were reset)"), except for a recent
    /// entry, whose totals are mostly those runs, diluted by the time before it began ([`ScanSpan::is_recent`]).
    pub fn how_often_of(&self, q: &QueryStat) -> String {
        let cadence = self.cadence(q);
        if let Some(s) = self.scans_of(q).filter(|_| !matches!(cadence, Cadence::Burst { .. })) {
            let now = s.runs_phrase();
            return match self.totals_runs(q, false).filter(|_| !s.is_recent(q) && s.runs_differ(self, q)) {
                Some(all) => format!("{now} ({all} over {})", self.window_phrase()),
                None => now,
            };
        }
        match cadence {
            Cadence::Counted(runs) => self.how_often(runs),
            burst @ Cadence::Burst { runs, .. } => {
                let comma = if runs == 1.0 { "," } else { "" };
                format!(
                    "{} {}{comma} {}",
                    crate::format::count(runs),
                    runs_noun(runs),
                    burst.when().unwrap_or_default()
                )
            }
            Cadence::DailyWithin { per_day, from, to } => {
                format!(
                    "about {} {} a day {}",
                    crate::format::count(per_day.round()),
                    runs_noun(per_day.round()),
                    days_when(&from, to.as_deref())
                )
            }
            Cadence::Daily(per_day) => {
                format!("about {} {} a day", crate::format::count(per_day.round()), runs_noun(per_day.round()))
            }
        }
    }

    /// Runs since the source began counting, said beside the scans' rate: "about 388 a day" ("about 388 runs a day"
    /// with `noun`), or the count where it is under one a day. `None` under a day of window, or one nobody can date.
    fn totals_runs(&self, q: &QueryStat, noun: bool) -> Option<String> {
        let per_day = self.per_day_measured(q.executions).filter(|_| !self.window_unknown())?;
        Some(if per_day >= 1.0 {
            let n = crate::format::count(per_day.round());
            let runs = match (noun, n == "1") {
                (false, _) => "",
                (true, true) => " run",
                (true, false) => " runs",
            };
            format!("about {n}{runs} a day")
        } else {
            format!("{} {}", crate::format::count(q.executions), runs_noun(q.executions))
        })
    }

    /// How a statement's runs spread over the window, so a rate a day is said only for runs that came that way: a
    /// burst (runs from a first to a last under a day apart, well inside the window: a job, an export, a test run) is
    /// when it came, in any window of an hour or more ([`RATE_MIN_HOURS`]); so are runs on a few of a longer window's
    /// days, at most half of them (an eval run on two afternoons), told by the timeline's buckets; fewer runs than the
    /// window has days are a count; runs over a stretch of a day or more that the window holds three times over are a
    /// rate over that stretch (an entry that began counting days ago, a statement that stopped). Otherwise, under a day
    /// of window or in one nobody can date, it is the count. A statement's first and last runs are the engine's own, or
    /// the buckets it ran in where those hold all its runs: pg_stat_statements keeps no last run (Culprit dates one
    /// from its scans, the latest it can have been), so without them it is taken to run to the window's end.
    pub fn cadence(&self, q: &QueryStat) -> Cadence {
        let runs = q.executions;
        let Some(end) = self.window_end.as_deref().and_then(crate::format::epoch_seconds) else {
            return match self.per_day_measured(runs) {
                Some(per_day) if per_day >= 1.0 => Cadence::Daily(per_day),
                _ => Cadence::Counted(runs),
            };
        };
        if self.window_unknown() || self.window_hours < RATE_MIN_HOURS {
            return Cadence::Counted(runs);
        }
        let window = self.window_hours * 3600.0;
        let start = end - window;
        let first = q.first_seen.as_deref().and_then(crate::format::epoch_seconds);
        let last = match self.kind() {
            Some(WorkloadKind::StatementStats) => None,
            _ => q.last_seen.as_deref().and_then(crate::format::epoch_seconds),
        };
        // Times that can't belong to this window's runs (after its end, or a last run an hour before its start: a
        // statement's times from another scan) say nothing of when they came.
        let consistent =
            first.is_none_or(|f| f <= end + 60.0) && last.is_none_or(|l| l <= end + 60.0 && l >= start - 3600.0);
        let (first, last) = if consistent {
            (first.map(|f| f.clamp(start, end)), last.map(|l| l.clamp(start, end)))
        } else {
            (None, None)
        };
        // The buckets it ran in narrow its first and last runs to them (and give pg_stat_statements a last one).
        let ran = self.ran_in(q, start, end);
        let within = |t: Option<f64>, (from, to): (f64, f64), edge: f64| Some(t.map_or(edge, |t| t.clamp(from, to)));
        let (first, last) = match &ran {
            Some(ran) => (within(first, ran[0], ran[0].0), within(last, ran[ran.len() - 1], ran[ran.len() - 1].1)),
            None => (first, last),
        };
        let iso = crate::format::iso_from_epoch;
        if let (Some(first), Some(last)) = (first, last)
            && last >= first
            && last - first < window * 0.5
            && last - first < PER_DAY_MIN_HOURS * 3600.0
        {
            return Cadence::Burst { runs, from: iso(first), to: iso(last), days: Vec::new() };
        }
        if self.window_hours < PER_DAY_MIN_HOURS {
            return Cadence::Counted(runs);
        }
        // Runs on a few days with silence between them: the buckets it ran in cover under half the time from its first
        // run to its last (a statement that began two days ago and runs on is a rate since then, not bursts).
        if let (Some(ran), Some(first), Some(last)) = (&ran, first, last) {
            let days = days_in(ran, first, last);
            let active: f64 = ran.iter().map(|(from, to)| to.min(last) - from.max(first)).filter(|s| *s > 0.0).sum();
            if days.len() >= 2 && days.len() as f64 * 2.0 <= self.window_hours / 24.0 && active < (last - first) * 0.5 {
                return Cadence::Burst { runs, from: iso(first), to: iso(last), days };
            }
        }
        let per_day = self.per_day(runs);
        if per_day < 1.0 {
            return Cadence::Counted(runs);
        }
        // An hour short of the window's end still runs, as far as the source can tell.
        let to = last.filter(|l| end - l > 3600.0);
        let from = first.filter(|f| f - start > 3600.0);
        let span = to.unwrap_or(end) - from.unwrap_or(start);
        if (from.is_some() || to.is_some()) && span >= PER_DAY_MIN_HOURS * 3600.0 && span < window * 0.75 {
            return Cadence::DailyWithin {
                per_day: runs * 86_400.0 / span,
                from: iso(from.unwrap_or(start)),
                to: to.map(iso),
            };
        }
        Cadence::Daily(per_day)
    }

    /// The stretches of the timeline statement `q` ran in (each bucket with runs of it, within `start`..`end`), oldest
    /// first, where the timeline holds all its runs: every bucket known for it, and its runs there adding up to its
    /// count. `None` otherwise (no timeline or trend, one that began after the statement did, a bucket it can't
    /// account for).
    fn ran_in(&self, q: &QueryStat, start: f64, end: f64) -> Option<Vec<(f64, f64)>> {
        let timeline = self.timeline.as_ref()?;
        let trend = q.trend.as_ref()?;
        let (mut counted, mut ran) = (0.0, Vec::new());
        for i in 0..timeline.len() {
            let runs = (*trend.executions.get(i)?)?;
            if runs <= 0.0 {
                continue;
            }
            let (from, to) = timeline.bounds(i);
            let (from, to) = (crate::format::epoch_seconds(from)?, crate::format::epoch_seconds(to)?);
            counted += runs;
            ran.push((from.clamp(start, end), to.clamp(start, end).max(from.clamp(start, end))));
        }
        (!ran.is_empty() && (counted - q.executions).abs() <= (q.executions * 0.01).max(0.5)).then_some(ran)
    }

    /// Runs a day for a statement, where its runs came that way ([`Workload::cadence`]): over Culprit's scans where
    /// they measured it on a source of running totals ([`Workload::scans_of`]), over the window, or over the stretch
    /// it ran in. `None` for a burst, for fewer runs than days, and under a day of window.
    pub fn runs_per_day_of(&self, q: &QueryStat) -> Option<f64> {
        let cadence = self.cadence(q);
        if !matches!(cadence, Cadence::Burst { .. })
            && let Some(s) = self.scans_of(q)
        {
            return Some(s.per_day(s.runs)).filter(|per_day| *per_day >= 1.0);
        }
        match cadence {
            Cadence::Daily(per_day) | Cadence::DailyWithin { per_day, .. } => Some(per_day),
            Cadence::Burst { .. } | Cadence::Counted(_) => None,
        }
    }

    /// `value`, counted over the window for statement `q` (its runs, its time, a part of either), as a rate a day for
    /// comparing with a threshold, at the pace its runs came ([`Workload::cadence`]), never extrapolated from a burst: a
    /// burst's total is what the day it ran held, and runs on a few days are their total over those days. On a source of
    /// running totals, at the pace Culprit's scans measured its runs where they did ([`Workload::scans_of`]). Runs over
    /// a stretch of the window are rated over that stretch, and steady runs over the window ([`Workload::rate_per_day`]).
    /// `None` under an hour of window ([`RATE_MIN_HOURS`]), where the total is all there is.
    pub fn rate_per_day_of(&self, q: &QueryStat, value: f64) -> Option<f64> {
        let cadence = self.cadence(q);
        if !matches!(cadence, Cadence::Burst { .. })
            && q.executions > 0.0
            && let Some(s) = self.scans_of(q)
        {
            return Some(value * s.per_day(s.runs) / q.executions);
        }
        match cadence {
            Cadence::Burst { days, .. } => Some(value / days.len().max(1) as f64),
            Cadence::DailyWithin { per_day, .. } if q.executions > 0.0 => Some(value * per_day / q.executions),
            _ => self.rate_per_day(value),
        }
    }

    /// What Culprit's own scans measured of statement `q`, on a source that keeps only running totals (the timeline
    /// built from the differences between scans): the steps that account for both the statement and the whole
    /// database, and no more for the statement than the database ran in a step (a counter misread in a saved scan, not
    /// a run). `None` under a day of such steps ([`PER_DAY_MIN_HOURS`]), so "a day" is never an extrapolation.
    pub fn over_scans(&self, q: &QueryStat) -> Option<ScanSpan> {
        let timeline = self.timeline.as_ref().filter(|t| t.source == TimelineSource::Scans && !self.windowed)?;
        let trend = q.trend.as_ref()?;
        let at = |s: &str| crate::format::epoch_seconds(s);
        let mut span = ScanSpan { hours: 0.0, runs: 0.0, time_ms: 0.0, workload_time_ms: 0.0, workload_runs: 0.0 };
        for i in 0..timeline.len() {
            let (from, to) = timeline.bounds(i);
            let (Some(from), Some(to)) = (at(from), at(to)) else { continue };
            let Some((time, runs)) = timeline.statement_bucket(trend, i).filter(|_| to > from) else { continue };
            let all_time = timeline.time_ms[i];
            span.hours += (to - from) / 3600.0;
            span.runs += runs;
            span.time_ms += time;
            span.workload_time_ms += all_time;
            span.workload_runs += timeline.executions.get(i).copied().unwrap_or(0.0);
        }
        (span.hours >= PER_DAY_MIN_HOURS).then_some(span)
    }

    /// What Culprit's own scans measured of statement `q` where that is what a person reads of it and what grades it:
    /// a day or more of them ([`Workload::over_scans`]) in which it ran, whether its totals since the source began
    /// counting say more or less. A statement that didn't run in them at all is read by its totals: what it took until
    /// it stopped, or a weekly job's average.
    pub fn scans_of(&self, q: &QueryStat) -> Option<ScanSpan> {
        self.over_scans(q).filter(|s| s.runs > 0.0)
    }

    /// The share of database time statement `q` is stated and graded at, on every surface (titles, impact, rank, the
    /// lists of statements): over Culprit's scans where they measured it ([`Workload::scans_of`]), otherwise its share
    /// of the window's totals, weighed as the source allows ([`Workload::share_of`]).
    pub fn statement_share(&self, q: &QueryStat) -> f64 {
        self.scans_of(q).map_or_else(|| self.share_of(q.total_time_ms, q), |s| s.share())
    }

    /// Statement `q`'s average run, the same way ([`Workload::statement_share`]).
    pub fn statement_mean_ms(&self, q: &QueryStat) -> f64 {
        self.scans_of(q).and_then(|s| s.mean_ms()).unwrap_or(q.mean_time_ms)
    }

    /// Statement `q`'s share of all statement runs, the same way ([`Workload::statement_share`]).
    pub fn statement_runs_share(&self, q: &QueryStat) -> f64 {
        match self.scans_of(q) {
            Some(s) => s.runs_share(),
            None if self.total_executions > 0.0 => (q.executions / self.total_executions).min(1.0),
            None => 0.0,
        }
    }

    /// Statement `q`'s database time a day, the same way ([`Workload::statement_share`]): over the scans, or over a
    /// window of a day or more where its runs came a day at a time ([`Workload::runs_per_day_of`]). `None` for a burst
    /// and under a day of either.
    pub fn statement_time_per_day(&self, q: &QueryStat) -> Option<f64> {
        if matches!(self.cadence(q), Cadence::Burst { .. }) {
            return None;
        }
        match self.scans_of(q) {
            Some(s) => Some(s.per_day(s.time_ms)),
            None => self.runs_per_day_of(q).and(self.per_day_measured(q.total_time_ms)),
        }
    }

    /// What statement `q` costs, as every list of statements says it (a brief's, an export's, the app's): its share, how
    /// often it ran and its average run, over Culprit's scans where they measured it ([`Workload::scans_of`]), with the
    /// totals since the source began counting beside them where those differ twofold.
    pub fn cost_of(&self, q: &QueryStat) -> StatementCost {
        let scans = self.scans_of(q);
        let (runs, counted) = match &scans {
            Some(s) if !matches!(self.cadence(q), Cadence::Burst { .. }) => (s.runs_phrase(), s.runs),
            _ => (self.how_often_of(q), q.executions),
        };
        StatementCost {
            query: q.id.clone(),
            share: self.statement_share(q),
            runs,
            runs_per_day: self.runs_per_day_of(q),
            counted,
            mean_ms: self.statement_mean_ms(q),
            over: scans.map(|s| s.phrase()),
            totals: scans.and_then(|s| self.totals_beside(q, &s)),
        }
    }

    /// The totals since the source began counting, where they say something else than Culprit's scans `s` (twice or
    /// half as much): "19% of database time, about 214 runs a day and 1.4 ms on average over the 1295 days since
    /// statistics were reset", each figure only where it differs. `None` for a recent entry ([`ScanSpan::is_recent`]),
    /// whose totals are those runs diluted by the time before it began, and where nothing differs.
    pub fn totals_beside(&self, q: &QueryStat, s: &ScanSpan) -> Option<String> {
        if s.is_recent(q) {
            return None;
        }
        let mut parts = Vec::new();
        if s.share_differs(self, q) {
            parts.push(format!("{} of database time", crate::format::percent(self.share_of(q.total_time_ms, q))));
        }
        // A burst's runs are when it ran, never a rate a day, on either side.
        if s.runs_differ(self, q)
            && !self.cadence(q).is_burst()
            && let Some(runs) = self.totals_runs(q, true)
        {
            parts.push(runs);
        }
        if s.mean_differs(q) {
            parts.push(format!("{} on average", crate::format::duration(q.mean_time_ms)));
        }
        (!parts.is_empty()).then(|| format!("{} over {}", crate::format::list(&parts), self.window_phrase()))
    }

    /// The source's running totals started at a time nobody can say (pg_stat_statements before PostgreSQL 14 keeps no
    /// reset time, and its statistics outlive restarts), or its log has no timestamps: `window_hours` is 0 and no rate
    /// can be derived from it.
    pub fn window_unknown(&self) -> bool {
        (!self.windowed || self.log.is_some()) && self.window_start.is_none() && self.window_hours <= 0.0
    }

    pub fn share(&self, time_ms: f64) -> f64 {
        if self.total_time_ms <= 0.0 {
            return 0.0;
        }
        (time_ms / self.total_time_ms).clamp(0.0, 1.0)
    }

    pub fn query(&self, id: &str) -> Option<&QueryStat> {
        self.queries.iter().find(|q| q.id == id)
    }
}

/// "run" or "runs", for a count.
fn runs_noun(runs: f64) -> &'static str {
    if (runs - 1.0).abs() < f64::EPSILON { "run" } else { "runs" }
}

/// How a statement's runs spread over its workload's window ([`Workload::cadence`]): what decides whether "a day" may
/// be said of them. Times are UTC, ISO 8601.
#[derive(Clone, Debug, PartialEq)]
pub enum Cadence {
    /// Runs a day over the window.
    Daily(f64),
    /// Runs a day over the stretch it ran in, from `from` to `to` (`None`: to the window's end).
    DailyWithin { per_day: f64, from: String, to: Option<String> },
    /// Runs that came in bursts well inside the window, never a rate a day: from `from` to `to` (its first run and its
    /// last), under a day apart; or, where `days` names them (UTC dates, `YYYY-MM-DD`), on those few of the window's
    /// days (an eval run on two afternoons, a batch on three nights).
    Burst { runs: f64, from: String, to: String, days: Vec<String> },
    /// Runs counted over the window: fewer than it has days, or a window too short (or undated) for a rate.
    Counted(f64),
}

impl Cadence {
    /// When the runs came, where that is part of how often they ran: "on Oct 11, 15:51–15:55 UTC" for a burst, "on Oct
    /// 11 and Oct 17" for runs on a few days, "since Oct 13" or "from Oct 11 to Oct 13" for a rate over a stretch of the
    /// window.
    pub fn when(&self) -> Option<String> {
        match self {
            // One run between two times is a point somewhere in them, not a run that lasted that long.
            Cadence::Burst { runs, from, to, days } if days.is_empty() && *runs <= 1.0 => Some(one_run_when(from, to)),
            Cadence::Burst { from, to, days, .. } if days.is_empty() => Some(span_when(from, to)),
            Cadence::Burst { days, .. } => Some(on_days(days)),
            Cadence::DailyWithin { from, to, .. } => Some(days_when(from, to.as_deref())),
            Cadence::Daily(_) | Cadence::Counted(_) => None,
        }
    }

    /// Whether the runs came in bursts ([`Cadence::Burst`]): said by when they came, graded on their total.
    pub fn is_burst(&self) -> bool {
        matches!(self, Cadence::Burst { .. })
    }
}

/// The UTC dates (`YYYY-MM-DD`) the stretches `ran` touch (epoch seconds, oldest first), the first narrowed to start at
/// `first` and the last to end at `last`: the days a statement ran on.
fn days_in(ran: &[(f64, f64)], first: f64, last: f64) -> Vec<String> {
    let mut days = std::collections::BTreeSet::new();
    for (i, &(from, to)) in ran.iter().enumerate() {
        let from = if i == 0 { first.max(from).min(to) } else { from };
        let to = if i + 1 == ran.len() { last.min(to).max(from) } else { to };
        // A stretch ends where the next begins: one ending at midnight ran on the day before.
        let to = if to > from { to - 1.0 } else { to };
        let mut day = (from / 86_400.0).floor();
        while day <= (to / 86_400.0).floor() {
            days.insert(crate::format::iso_from_epoch(day * 86_400.0)[..10].to_string());
            day += 1.0;
        }
    }
    days.into_iter().collect()
}

/// The days runs came on (UTC dates, `YYYY-MM-DD`): "on Oct 11 and Oct 17", "on Oct 11, Oct 13 and Oct 17", and past
/// three "on 4 days between Oct 11 and Oct 17".
fn on_days(days: &[String]) -> String {
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    match days {
        [] => String::new(),
        [one] => format!("on {}", day(one)),
        [first @ .., last] if days.len() <= 3 => {
            format!("on {} and {}", first.iter().map(|d| day(d)).collect::<Vec<_>>().join(", "), day(last))
        }
        [first, .., last] => format!("on {} days between {} and {}", days.len(), day(first), day(last)),
    }
}

/// What Culprit's own scans measured of one statement ([`Workload::over_scans`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanSpan {
    /// Hours the steps between scans cover.
    pub hours: f64,
    pub runs: f64,
    pub time_ms: f64,
    /// The whole database's time in the same steps.
    pub workload_time_ms: f64,
    /// The whole database's runs in the same steps.
    pub workload_runs: f64,
}

impl ScanSpan {
    /// The statement's share of the database's runs in these steps.
    pub fn runs_share(&self) -> f64 {
        if self.workload_runs <= 0.0 { 0.0 } else { (self.runs / self.workload_runs).clamp(0.0, 1.0) }
    }

    /// `value`, counted over these steps, a day.
    pub fn per_day(&self, value: f64) -> f64 {
        value * 24.0 / self.hours
    }

    /// The statement's share of the database's time in these steps.
    pub fn share(&self) -> f64 {
        if self.workload_time_ms <= 0.0 { 0.0 } else { (self.time_ms / self.workload_time_ms).clamp(0.0, 1.0) }
    }

    /// "72 hours of Culprit's scans": the steps, as sentences name them (they need not be every scan's).
    pub fn phrase(&self) -> String {
        format!("{} of Culprit's scans", self.length())
    }

    /// How long the steps cover: hours under four days (89 hours are not "4 days"), days to a tenth after.
    pub fn length(&self) -> String {
        if self.hours < 96.0 {
            return format!("{} hours", self.hours.round());
        }
        let tenths = format!("{:.1}", (self.hours / 2.4).floor() / 10.0);
        format!("{} days", tenths.strip_suffix(".0").unwrap_or(&tenths))
    }

    /// The statement's average run in these steps. `None` without a run.
    pub fn mean_ms(&self) -> Option<f64> {
        (self.runs > 0.0).then(|| self.time_ms / self.runs)
    }

    /// "about 1.7K runs a day over 72 hours of Culprit's scans", or the count under one a day ("3 runs over 72 hours
    /// of Culprit's scans").
    pub fn runs_phrase(&self) -> String {
        let per_day = self.per_day(self.runs);
        if per_day >= 1.0 {
            format!(
                "about {} {} a day over {}",
                crate::format::count(per_day.round()),
                runs_noun(per_day.round()),
                self.phrase()
            )
        } else {
            format!("{} {} over {}", crate::format::count(self.runs), runs_noun(self.runs), self.phrase())
        }
    }

    /// Whether statement `q`'s entry is recent: at least half of the runs its totals hold came in these steps, so the
    /// rest of the window before it began counting dilutes every average of them (an entry the application started
    /// sending days ago, or one evicted and created again).
    pub fn is_recent(&self, q: &QueryStat) -> bool {
        self.runs >= q.executions * 0.5
    }

    /// Whether these steps' runs a day are twice or half statement `q`'s since the source began counting. False when
    /// those can't be said a day (under a day of window, or one nobody can date).
    pub fn runs_differ(&self, workload: &Workload, q: &QueryStat) -> bool {
        let all = workload.per_day_measured(q.executions).filter(|_| !workload.window_unknown());
        all.is_some_and(|all| apart(self.per_day(self.runs), all))
    }

    /// Whether the statement's share of database time in these steps is twice or half its share of the totals.
    pub fn share_differs(&self, workload: &Workload, q: &QueryStat) -> bool {
        apart(self.share(), workload.share_of(q.total_time_ms, q))
    }

    /// Whether its average run in these steps is twice or half its average since the source began counting.
    pub fn mean_differs(&self, q: &QueryStat) -> bool {
        self.mean_ms().is_some_and(|now| apart(now, q.mean_time_ms))
    }

    /// Whether the statement's share of the database's runs in these steps is twice or half its share of the totals'.
    pub fn runs_share_differs(&self, workload: &Workload, q: &QueryStat) -> bool {
        workload.total_executions > 0.0 && apart(self.runs_share(), q.executions / workload.total_executions)
    }
}

/// Two figures twice or half as much as each other: what a sentence gives both of.
fn apart(now: f64, all: f64) -> bool {
    now >= all * 2.0 || now * 2.0 <= all
}

/// What a statement costs, as the lists of statements say it ([`Workload::cost_of`]).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct StatementCost {
    /// The statement's id in the workload.
    pub query: String,
    /// Its share of database time ([`Workload::statement_share`]).
    pub share: f64,
    /// How often it ran: "about 1.7K runs a day over 72 hours of Culprit's scans", or [`Workload::how_often_of`].
    pub runs: String,
    /// Its runs a day where its runs came that way ([`Workload::runs_per_day_of`]).
    pub runs_per_day: Option<f64>,
    /// The runs `runs` counts where it says no rate a day (a burst, a few runs): over Culprit's scans where they
    /// measured it, otherwise over the window.
    pub counted: f64,
    /// Its average run ([`Workload::statement_mean_ms`]).
    pub mean_ms: f64,
    /// What these figures are over where Culprit's scans measured them: "72 hours of Culprit's scans". `None`: the
    /// workload's window.
    pub over: Option<String>,
    /// The totals since the source began counting, where they differ twofold ([`Workload::totals_beside`]).
    pub totals: Option<String>,
}

/// When runs came, from `from` to `to` (ISO 8601): "on Oct 11 at 13:58 UTC" for one moment, "on Oct 11, 15:51–15:55
/// UTC" within a day, "between Oct 11, 22:10 and Oct 12, 03:10 UTC" across midnight.
pub fn span_when(from: &str, to: &str) -> String {
    let (Some(a), Some(b)) = (crate::format::epoch_seconds(from), crate::format::epoch_seconds(to)) else {
        return format!("between {from} and {to}");
    };
    let (a, b) = (crate::format::iso_from_epoch(a), crate::format::iso_from_epoch(b));
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    let time = |iso: &str| iso.get(11..16).unwrap_or_default().to_string();
    if time(&a) == time(&b) && day(&a) == day(&b) {
        format!("on {} at {} UTC", day(&a), time(&a))
    } else if day(&a) == day(&b) {
        format!("on {}, {}–{} UTC", day(&a), time(&a), time(&b))
    } else {
        format!("between {}, {} and {}, {} UTC", day(&a), time(&a), day(&b), time(&b))
    }
}

/// When one run came, from the bounds that hold it: "on Oct 11 at 13:58 UTC" where they meet, else "sometime between
/// 08:41 and 09:28 UTC on Oct 11" ("sometime between Oct 11, 23:10 and Oct 12, 00:40 UTC" across midnight).
fn one_run_when(from: &str, to: &str) -> String {
    let (Some(a), Some(b)) = (crate::format::epoch_seconds(from), crate::format::epoch_seconds(to)) else {
        return format!("sometime between {from} and {to}");
    };
    let (a, b) = (crate::format::iso_from_epoch(a), crate::format::iso_from_epoch(b));
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    let time = |iso: &str| iso.get(11..16).unwrap_or_default().to_string();
    if time(&a) == time(&b) && day(&a) == day(&b) {
        format!("on {} at {} UTC", day(&a), time(&a))
    } else if day(&a) == day(&b) {
        format!("sometime between {} and {} UTC on {}", time(&a), time(&b), day(&a))
    } else {
        format!("sometime between {}, {} and {}, {} UTC", day(&a), time(&a), day(&b), time(&b))
    }
}

/// "since Oct 13" for runs to the window's end, "from Oct 11 to Oct 13" otherwise.
fn days_when(from: &str, to: Option<&str>) -> String {
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    match to {
        Some(to) => format!("from {} to {}", day(from), day(to)),
        None => format!("since {}", day(from)),
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct QueryStat {
    /// A CTE write's unambiguous base table, resolved by shared SQL analysis. Absent when unknown or unnecessary.
    #[serde(default)]
    pub resolved_write_target: Option<String>,
    pub id: String,
    pub query_hash: Option<String>,
    pub text: String,
    /// Parameter declarations sent with the statement, e.g. `@P1 nvarchar(4000)`; they explain implicit conversions.
    pub parameters: Option<String>,
    pub object_name: Option<String>,
    pub executions: f64,
    pub total_time_ms: f64,
    pub mean_time_ms: f64,
    pub max_time_ms: Option<f64>,
    pub stddev_time_ms: Option<f64>,
    pub cpu_time_ms: Option<f64>,
    pub rows: Option<f64>,
    pub logical_reads: Option<f64>,
    pub physical_reads: Option<f64>,
    pub temp_writes: Option<f64>,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub plans: Vec<PlanVariant>,
    pub plan: Option<Plan>,
    pub waits: Vec<WaitStat>,
    /// This query's part of each bucket of the workload's timeline (only the heaviest queries carry one).
    #[serde(default)]
    pub trend: Option<QueryTrend>,
    /// How run times spread beyond the average, as far as the engine keeps it.
    #[serde(default)]
    pub spread: Option<Spread>,
    /// Failed runs, planning, JIT, log volume and result size, as far as the engine keeps them.
    #[serde(default)]
    pub costs: Option<crate::signals::QueryCosts>,
    /// Rows the statement changed (inserted, updated or deleted), where the engine counts them apart from the rows it
    /// returned (`rows`): MySQL and MariaDB's performance_schema. `None` where the engine doesn't separate them.
    #[serde(default)]
    pub rows_affected: Option<f64>,
    /// Which of `plans` the attached `plan` is (Query Store's plan_id): the plan still in use (the heaviest of the
    /// variants no newer variant replaced), not merely the heaviest over the window. `None` when the source keeps one
    /// plan per statement.
    #[serde(default)]
    pub plan_id: Option<String>,
}

/// How a statement's run times spread. Every engine keeps something different: SQL Server and Postgres the fastest
/// run, MySQL percentiles and a run-time histogram (8.0.19+). Missing values are `None`, never zero.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Spread {
    pub min_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    /// `[low_ms, high_ms, runs]` per bucket, fastest first (MySQL).
    #[serde(default)]
    pub histogram: Vec<[f64; 3]>,
    /// Time spent waiting for locks across all runs (MySQL).
    #[serde(default)]
    pub lock_ms: Option<f64>,
}

impl Spread {
    /// Percentiles never above the slowest run: MySQL reports them as bucket upper edges, which can overshoot it.
    pub fn clamp_to(&mut self, max_ms: Option<f64>) {
        let Some(max) = max_ms.filter(|m| m.is_finite() && *m > 0.0) else { return };
        for p in [&mut self.p50_ms, &mut self.p95_ms, &mut self.p99_ms] {
            if let Some(v) = p.as_mut() {
                *v = v.min(max);
            }
        }
    }
}

/// Where a timeline's buckets come from.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TimelineSource {
    /// Query Store's runtime-stats intervals, folded into equal buckets.
    #[default]
    QueryStore,
    /// Differences between Culprit's own saved scans of a source that only keeps running totals.
    Scans,
    /// Runs logged one by one (MySQL and MariaDB's slow query log table), bucketed by when they ran.
    SlowLog,
}

/// Database time and executions per bucket. Buckets are equal for Query Store and fall between scans otherwise.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct Timeline {
    pub source: TimelineSource,
    /// Start of each bucket (UTC, ISO 8601). Bucket `i` ends where bucket `i + 1` starts; the last one ends at `end`.
    pub starts: Vec<String>,
    pub end: String,
    pub time_ms: Vec<f64>,
    pub executions: Vec<f64>,
    /// Buckets the source can't account for in full, by index: its running totals shrank between two scans without a
    /// reset (entries evicted from pg_stat_statements, plans dropped from the cache), so `time_ms` and `executions`
    /// there hold only the growth of the statements Culprit tracks, a lower bound. `None` when every bucket is known.
    #[serde(default)]
    pub unknown: Option<Vec<bool>>,
}

impl Timeline {
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Start and end of bucket `i`.
    pub fn bounds(&self, i: usize) -> (&str, &str) {
        let end = self.starts.get(i + 1).unwrap_or(&self.end);
        (self.starts[i].as_str(), end.as_str())
    }

    /// Whether bucket `i` holds only part of what ran in it (see [`Timeline::unknown`]).
    pub fn is_unknown(&self, i: usize) -> bool {
        self.unknown.as_ref().and_then(|u| u.get(i).copied()).unwrap_or(false)
    }

    /// What a statement ran in bucket `i`, as `(time ms, runs)`, where the bucket accounts for it: `None` where the
    /// statement wasn't tracked, in a bucket the source can't account for in full ([`Timeline::unknown`]), and where
    /// the statement ran more than the whole database did (a counter misread in a saved scan, not a run). Every reading
    /// of a statement's trend (its rate over Culprit's scans, a regression, the app's charts) takes its buckets from
    /// here; the app mirrors it (`statementBucket` in `app/src/components/Trends.tsx`).
    pub fn statement_bucket(&self, trend: &QueryTrend, i: usize) -> Option<(f64, f64)> {
        let time = (*trend.time_ms.get(i)?)?;
        let runs = (*trend.executions.get(i)?)?;
        let (all_time, all_runs) = (*self.time_ms.get(i)?, *self.executions.get(i)?);
        (!self.is_unknown(i) && time <= all_time + 0.5 && runs <= all_runs + 0.5).then_some((time, runs))
    }
}

/// One query's database time and executions in each bucket of the workload timeline. `None` marks a bucket the
/// source can't account for (an earlier scan didn't track this query, or its counters were reset).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct QueryTrend {
    pub time_ms: Vec<Option<f64>>,
    pub executions: Vec<Option<f64>>,
}

impl QueryTrend {
    /// Average duration in bucket `i`, when the query ran in it.
    pub fn mean_ms(&self, i: usize) -> Option<f64> {
        let time = (*self.time_ms.get(i)?)?;
        let runs = (*self.executions.get(i)?)?;
        (runs > 0.0).then(|| time / runs)
    }
}

impl QueryStat {
    /// The measured runs of the attached plan, where the source identifies its variants. A statement's average over
    /// all its plans says nothing about whether this particular plan runs cheaply.
    pub fn captured_plan_variant(&self) -> Option<&PlanVariant> {
        let id = self.plan_id.as_ref()?;
        self.plans.iter().find(|p| &p.id == id)
    }

    /// What its runs took, where an average would hide it: its one run's time, or two runs' times, the longest first,
    /// where one took twice the other or more (an average of 5.2 s for runs of 10.5 s and 0.3 ms). `None` for more
    /// runs, for two alike, and where the engine keeps no longest run.
    pub fn few_runs(&self) -> Option<Vec<f64>> {
        if self.executions == 1.0 {
            return Some(vec![self.total_time_ms]);
        }
        if self.executions != 2.0 || self.total_time_ms <= 0.0 {
            return None;
        }
        let longest = self.max_time_ms?.clamp(0.0, self.total_time_ms);
        let other = self.total_time_ms - longest;
        (longest >= other * 2.0).then(|| vec![longest, other])
    }

    /// [`QueryStat::few_runs`] in words, for a sentence that says what its runs took in place of their average: "24.3
    /// s" for one run, "10.5 s and 0.3 ms" for two.
    pub fn few_runs_took(&self) -> Option<String> {
        let runs = self.few_runs()?;
        Some(runs.iter().map(|ms| crate::format::duration(*ms)).collect::<Vec<_>>().join(" and "))
    }

    pub fn rows_per_execution(&self) -> Option<f64> {
        let rows = self.rows?;
        (self.executions > 0.0).then(|| rows / self.executions)
    }

    /// Whether the statement changes rows (INSERT, UPDATE, DELETE, MERGE, REPLACE), from its text.
    pub fn writes_rows(&self) -> bool {
        let lower = crate::sql::without_parameter_list(&self.text)
            .trim_start_matches(|c: char| c.is_whitespace() || c == '(')
            .to_ascii_lowercase();
        ["insert", "update", "delete", "merge", "replace"].iter().any(|v| lower.starts_with(v))
    }

    /// Rows a run handled: for a write, the rows it changed where the engine counts them apart from rows returned
    /// (MySQL sends a write's client no rows, so `rows` is 0 for every INSERT); otherwise the rows it returned.
    pub fn rows_handled_per_execution(&self) -> Option<f64> {
        let rows = if self.writes_rows() { self.rows_affected.or(self.rows)? } else { self.rows? };
        (self.executions > 0.0).then(|| rows / self.executions)
    }

    pub fn reads_per_execution(&self) -> Option<f64> {
        let reads = self.logical_reads?;
        (self.executions > 0.0).then(|| reads / self.executions)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanVariant {
    pub id: String,
    pub executions: f64,
    pub total_time_ms: f64,
    pub mean_time_ms: f64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub is_forced: bool,
    #[serde(default)]
    pub min_time_ms: Option<f64>,
    #[serde(default)]
    pub max_time_ms: Option<f64>,
    #[serde(default)]
    pub stddev_time_ms: Option<f64>,
    /// The parameter values this plan was compiled for, as the engine reports them (SQL Server showplan).
    #[serde(default)]
    pub compiled: Option<String>,
    /// SQL Server 2022 parameter-sensitive plans: the variant query this plan belongs to.
    #[serde(default)]
    pub variant_query: Option<String>,
    /// CPU time and logical reads across its runs, where the source keeps them per plan (Query Store): whether a
    /// slower plan does more work, or only waited longer while it ran.
    #[serde(default)]
    pub cpu_time_ms: Option<f64>,
    #[serde(default)]
    pub logical_reads: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct TableStat {
    pub schema: String,
    pub name: String,
    pub rows: Option<f64>,
    pub total_bytes: Option<f64>,
    pub data_bytes: Option<f64>,
    pub index_bytes: Option<f64>,
    pub has_primary_key: bool,
    pub is_heap: Option<bool>,
    pub seq_scans: Option<f64>,
    pub seq_rows_read: Option<f64>,
    pub index_scans: Option<f64>,
    pub inserts: Option<f64>,
    pub updates: Option<f64>,
    pub deletes: Option<f64>,
    pub dead_rows: Option<f64>,
    pub modifications_since_analyze: Option<f64>,
    pub last_vacuum: Option<String>,
    pub last_analyze: Option<String>,
    pub forwarded_records: Option<f64>,
    pub xid_age: Option<f64>,
    /// Reads of a heap that followed a forwarding pointer to the row's new page, since the table's metadata was cached
    /// (SQL Server).
    #[serde(default)]
    pub forwarded_fetches: Option<f64>,
    /// How updates land on its pages and how much is empty space (PostgreSQL).
    #[serde(default)]
    pub storage: Option<crate::signals::TableStorage>,
    /// Bytes of the table's own rows (its heap or clustered index) kept in the rows' pages. With `lob_bytes` and
    /// `overflow_bytes`, how its data splits between the rows and the large values kept apart from them. `None` where
    /// the engine doesn't say.
    #[serde(default)]
    pub in_row_bytes: Option<f64>,
    /// Bytes of large values kept apart from the rows: SQL Server's LOB pages (`nvarchar(max)`, `varbinary(max)`,
    /// `xml`…), PostgreSQL's TOAST table.
    #[serde(default)]
    pub lob_bytes: Option<f64>,
    /// Bytes of variable-length values pushed off a row that outgrew its page (SQL Server row-overflow pages).
    #[serde(default)]
    pub overflow_bytes: Option<f64>,
    /// The partitioned table this one is a partition of (`schema.table`, PostgreSQL). Its rows, sizes and counters are
    /// the partition's own; judge the table as a whole at the parent. `None` for a table that isn't a partition.
    #[serde(default)]
    pub partition_of: Option<String>,
    /// The latest its usage counters (scans, reads, writes) may have started over (UTC), when a scan saw them shrink
    /// since the scan before without the server restarting: a statistics reset for the table, a database taken offline,
    /// restored or failed over. What they count covers the time since then at least. `None`: as old as the server's.
    #[serde(default)]
    pub counters_since: Option<String>,
    // The engine's table kind (SQLite: "table" or "virtual"). Absent in older scans and where not collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    // The table's original CREATE statement, where collected. Virtual tables must keep their module and options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

impl TableStat {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    pub fn is_virtual(&self) -> bool {
        self.kind.as_deref().is_some_and(|kind| kind.eq_ignore_ascii_case("virtual"))
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct IndexStat {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub kind: String,
    pub columns: Vec<String>,
    pub include: Vec<String>,
    pub filter: Option<String>,
    pub is_unique: bool,
    pub is_primary_key: bool,
    pub is_clustered: bool,
    pub is_disabled: bool,
    pub size_bytes: Option<f64>,
    pub reads: Option<f64>,
    pub writes: Option<f64>,
    pub seeks: Option<f64>,
    pub scans: Option<f64>,
    pub lookups: Option<f64>,
    pub last_read: Option<String>,
    pub definition: Option<String>,
    /// Lock and latch waits on this index (SQL Server).
    #[serde(default)]
    pub contention: Option<crate::signals::IndexContention>,
    /// The latest it may have been created (UTC), set by a scan only when something says it may be new: an earlier
    /// scan saw its table without it, or the database dated a change to its table that no earlier scan was there to
    /// see, or its usage counters started over (`counters_since`). Its usage counters cover the time since at least.
    /// `None`: as old as the counters, as far as anyone knows.
    #[serde(default)]
    pub created_at_latest: Option<String>,
    /// The partitioned index this one is a partition of (`schema.index`, PostgreSQL): every partition of a table gets its
    /// own copy of the parent's index. Judge it (unused, duplicate) at the parent. `None` for an index of its own.
    #[serde(default)]
    pub partition_of: Option<String>,
    /// Per key column, what the index compares it by when that isn't the column type's default: an operator class
    /// (`text_pattern_ops`, `jsonb_path_ops`) and/or a collation (`COLLATE "C"`), empty for a key that uses the
    /// defaults. Two indexes on the same columns with different ones serve different queries, so neither duplicates
    /// the other. `None` when every key uses the defaults, or the engine has no such thing.
    #[serde(default)]
    pub opclasses: Option<Vec<String>>,
    /// The latest its usage counters may have started over (UTC), when a scan saw them shrink or vanish since the scan
    /// before without the server restarting: a database taken offline, restored or failed over (SQL Server keeps index
    /// usage per database), an ALTER or OPTIMIZE of its table (MySQL), a statistics reset. No reads says nothing about
    /// the time before it; `created_at_latest` moves to it too, so a check that waits for a young index waits for these
    /// counters as well. `None`: as old as the server's counters, as far as anyone knows.
    #[serde(default)]
    pub counters_since: Option<String>,
}

impl IndexStat {
    pub fn qualified_table(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }

    pub fn key_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| strip_direction(c)).collect()
    }
}

pub fn strip_direction(column: &str) -> &str {
    column.strip_suffix(" DESC").unwrap_or(column)
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ColumnInfo {
    pub schema: String,
    pub table: String,
    pub name: String,
    /// Engine type as the database spells it, with length/precision: `varchar(20)`, `numeric(12,2)`, `timestamptz`.
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    /// Identity / auto-increment / serial-backed column.
    pub generated: bool,
    pub ordinal: u32,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ForeignKey {
    pub name: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct MissingIndex {
    pub schema: String,
    pub table: String,
    pub equality: Vec<String>,
    pub inequality: Vec<String>,
    pub include: Vec<String>,
    pub seeks: f64,
    pub scans: f64,
    pub avg_query_cost: f64,
    pub avg_impact_pct: f64,
    pub last_used: Option<String>,
    pub query_hashes: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct StatisticsInfo {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub last_updated: Option<String>,
    pub rows: Option<f64>,
    pub rows_sampled: Option<f64>,
    pub modifications: Option<f64>,
    pub auto_created: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct WaitStat {
    pub category: String,
    pub wait_type: Option<String>,
    pub wait_ms: f64,
    pub tasks: Option<f64>,
}

/// The setting a provider adds when the connection reached a replica (a readable secondary, a standby, a replicating
/// MySQL): its value says which kind. A replica's usage counters count only the reads run on it, so they say nothing
/// about what the primary's workload uses.
pub const REPLICA_SETTING: &str = "replica";

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Setting {
    pub name: String,
    pub value: String,
    pub default_value: Option<String>,
    pub scope: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ResourceSample {
    pub at: String,
    pub cpu_pct: Option<f64>,
    pub data_io_pct: Option<f64>,
    pub log_write_pct: Option<f64>,
    pub memory_pct: Option<f64>,
    pub workers_pct: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CollectorNote {
    pub collector: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Snapshot {
    pub format: u32,
    pub id: String,
    pub connection: String,
    pub environment: Environment,
    pub taken_at: String,
    pub duration_ms: f64,
    pub server: ServerInfo,
    pub workload: Option<Workload>,
    pub tables: Vec<TableStat>,
    pub indexes: Vec<IndexStat>,
    #[serde(default)]
    pub columns: Vec<ColumnInfo>,
    pub foreign_keys: Vec<ForeignKey>,
    pub missing_indexes: Vec<MissingIndex>,
    pub statistics: Vec<StatisticsInfo>,
    pub waits: Vec<WaitStat>,
    pub resources: Vec<ResourceSample>,
    pub settings: Vec<Setting>,
    pub notes: Vec<CollectorNote>,
    /// Pressure signals from cheap counters (compiles, memory grants, cache hits, open transactions).
    #[serde(default)]
    pub pressure: Option<crate::waits::Pressure>,
    /// What the scan asked for; the workload's own window says what the source could actually cover.
    #[serde(default)]
    pub options: Option<SnapshotOptions>,
    /// Statements that differ only in literal values (Query Store query_hash, pg_stat_statements entries).
    #[serde(default)]
    pub query_families: Vec<crate::advice::QueryFamily>,
    /// Single-use plans in the plan cache (SQL Server).
    #[serde(default)]
    pub plan_cache: Option<crate::advice::PlanCacheUse>,
    /// Query Store's tuning recommendations, forced plans and hints (SQL Server).
    #[serde(default)]
    pub tuning: Option<crate::advice::TuningAdvice>,
    /// Deadlocks the engine recorded in the last day (graphs on SQL Server and MySQL, a counter on Postgres).
    #[serde(default)]
    pub deadlocks: Option<crate::deadlock::DeadlockReport>,
    /// Constraints the optimizer can't rely on: unchecked against existing rows or switched off.
    #[serde(default)]
    pub constraints: Vec<crate::signals::ConstraintIssue>,
    /// Column histograms (MySQL 8, MariaDB); `None` when the engine has none or they couldn't be read.
    #[serde(default)]
    pub histograms: Option<Vec<crate::signals::ColumnHistogram>>,
    /// The lock escalations an earlier scan counted (SQL Server), set from the scans Culprit keeps or a previous
    /// digest: what tells escalations since then from old ones. `None` without an earlier scan to compare with.
    #[serde(default)]
    pub escalations_before: Option<crate::signals::EscalationBaseline>,
    /// Per-index lock counters read by an earlier scan of this database; absent without comparable history.
    #[serde(default)]
    pub contention_before: Option<crate::signals::ContentionBaseline>,
    /// pg_stat_statements' eviction rounds an earlier scan counted, set from the scans Culprit keeps: what tells rounds
    /// since then from old ones, since the count outlives restarts and a higher limit. `None` without an earlier scan
    /// that read them.
    #[serde(default)]
    pub evictions_before: Option<crate::signals::CountBaseline>,
    /// How big each table's data was at an earlier scan of the same database, set from the scans Culprit keeps: what
    /// tells how fast a table grows. `None` without an earlier scan.
    #[serde(default)]
    pub sizes_before: Option<crate::signals::SizeBaseline>,
}

impl Snapshot {
    /// Why this scan has no query statistics ("Query Store is OFF on this database."), or `None` when it has them.
    pub fn no_workload_reason(&self) -> Option<String> {
        if self.workload.is_some() {
            return None;
        }
        Some(
            self.server
                .missing
                .iter()
                .find(|m| m.capability.is_workload_source())
                .map(|m| m.reason.clone())
                .unwrap_or_else(|| "This scan has no query history.".into()),
        )
    }

    pub fn table(&self, schema: &str, name: &str) -> Option<&TableStat> {
        self.tables.iter().find(|t| t.schema.eq_ignore_ascii_case(schema) && t.name.eq_ignore_ascii_case(name))
    }

    pub fn indexes_on<'a>(&'a self, schema: &'a str, table: &'a str) -> impl Iterator<Item = &'a IndexStat> + 'a {
        self.indexes
            .iter()
            .filter(move |i| i.schema.eq_ignore_ascii_case(schema) && i.table.eq_ignore_ascii_case(table))
    }

    pub fn setting(&self, name: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn queries(&self) -> &[QueryStat] {
        self.workload.as_ref().map(|w| w.queries.as_slice()).unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Session {
    pub id: String,
    pub user: Option<String>,
    pub application: Option<String>,
    pub host: Option<String>,
    pub database: Option<String>,
    pub state: String,
    pub command: Option<String>,
    pub wait: Option<String>,
    pub wait_ms: Option<f64>,
    pub blocked_by: Option<String>,
    pub elapsed_ms: Option<f64>,
    pub transaction_ms: Option<f64>,
    pub cpu_ms: Option<f64>,
    pub reads: Option<f64>,
    pub sql: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct LiveState {
    pub taken_at: String,
    pub sessions: Vec<Session>,
    pub waits: Vec<WaitStat>,
    /// What Live may do to other sessions on this connection; empty on production connections.
    #[serde(default)]
    pub session_actions: Vec<SessionAction>,
    /// Why `session_actions` is empty, in plain language.
    #[serde(default)]
    pub session_actions_note: Option<String>,
    /// Who blocks whom, as chains under each head blocker, when anyone waits on a lock.
    #[serde(default)]
    pub blocking: Option<crate::blocking::Blocking>,
}

/// A numeric session id from `live()` (SQL Server spid, Postgres pid, MySQL thread id). Ids are spliced into `KILL`, so
/// anything but digits is refused.
pub fn session_number(session: &str) -> crate::error::Result<u64> {
    let session = session.trim();
    if session.is_empty() || session.len() > 19 || !session.bytes().all(|b| b.is_ascii_digit()) {
        return Err(crate::error::Error::invalid(format!("'{session}' is not a session id")));
    }
    session.parse().map_err(|_| crate::error::Error::invalid(format!("'{session}' is not a session id")))
}

/// Something Live can do to another session. Never offered on production connections.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SessionAction {
    /// Stop the statement the session is running; the session stays connected.
    Cancel,
    /// Close the session; its open transaction rolls back.
    Terminate,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
    pub elapsed_ms: f64,
    pub messages: Vec<String>,
}

pub type Properties = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    #[test]
    fn a_scan_without_query_history_says_why() {
        let mut snapshot: super::Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
        assert_eq!(snapshot.no_workload_reason(), None);
        snapshot.workload = None;
        snapshot.server.missing.retain(|m| m.capability != super::Capability::QueryStore);
        assert_eq!(snapshot.no_workload_reason().as_deref(), Some("This scan has no query history."));
        snapshot.server.missing.push(super::MissingCapability {
            capability: super::Capability::QueryStore,
            reason: "Query Store is OFF on this database.".into(),
            fix: None,
        });
        assert_eq!(snapshot.no_workload_reason().as_deref(), Some("Query Store is OFF on this database."));
    }

    #[test]
    fn percentiles_never_pass_the_slowest_run() {
        let mut s = super::Spread { p50_ms: Some(10.0), p95_ms: Some(40.0), p99_ms: Some(43.7), ..Default::default() };
        s.clamp_to(Some(43.4));
        assert_eq!((s.p50_ms, s.p95_ms, s.p99_ms), (Some(10.0), Some(40.0), Some(43.4)));
        s.clamp_to(None);
        assert_eq!(s.p99_ms, Some(43.4));
    }

    /// The app reads each engine's facts (abilities, session actions, the reasons it refuses) from the engine files
    /// themselves: written next to the generated types (`cargo test -p culprit-core`), so the browser demo and every
    /// view answer as the engine does and can't drift from it.
    #[test]
    fn engine_facts_are_exported_for_the_app() {
        let table: std::collections::BTreeMap<String, crate::engines::EngineFacts> =
            Engine::ALL.iter().map(|e| (wire_name(e), e.facts())).collect();
        assert_eq!(table.len(), Engine::ALL.len());
        // Every ability an engine lacks has its own reason.
        for e in Engine::ALL {
            for a in Ability::ALL {
                if !e.abilities().has(a) {
                    assert!(!e.spec().refusal(a).message.is_empty(), "{e} {a:?}");
                }
            }
        }
        if let Ok(dir) = std::env::var("TS_RS_EXPORT_DIR") {
            // In the engines' own order (`Engine::ALL`), which the app keeps wherever it lists them.
            struct InOrder;
            impl serde::Serialize for InOrder {
                fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    use serde::ser::SerializeMap;
                    let mut map = serializer.serialize_map(Some(Engine::ALL.len()))?;
                    for e in Engine::ALL {
                        map.serialize_entry(&wire_name(&e), &e.facts())?;
                    }
                    map.end()
                }
            }
            let text = serde_json::to_string_pretty(&InOrder).expect("facts") + "\n";
            std::fs::write(std::path::Path::new(&dir).join("engines.json"), text).expect("app/src/gen/engines.json");
        }
    }

    #[test]
    fn how_often_never_extrapolates_a_short_window() {
        let w = |hours: f64, windowed: bool| Workload {
            source: "Query Store".into(),
            kind: Some(WorkloadKind::QueryStore),
            windowed,
            window_start: None,
            window_end: None,
            window_hours: hours,
            total_time_ms: 0.0,
            total_cpu_ms: None,
            total_executions: 0.0,
            query_count: 0.0,
            queries: vec![],
            timeline: None,
            untracked_time_ms: None,
            hourly_time_ms: None,
            min_logged_ms: None,
            log: None,
        };
        // 4 runs in 10 hours is not "~9.6 runs a day".
        assert_eq!(w(10.0, true).how_often(4.0), "4 runs over the last 10 hours");
        assert_eq!(w(10.0, true).per_day_measured(4.0), None);
        assert_eq!(w(0.5, false).how_often(1.0), "1 run over the 30 minutes since statistics were reset");
        assert_eq!(w(48.0, true).how_often(4200.0), "about 2.1K runs a day");
        // A cumulative source that can't say when it started counting has no window to put a rate on.
        let mut unknown = w(0.0, false);
        assert!(unknown.window_unknown());
        assert_eq!(unknown.how_often(3.0), "3 runs over the time since statistics were last reset");
        assert_eq!(unknown.rate_per_day(3.0), None);
        unknown.window_start = Some("2026-10-01T00:00:00Z".into());
        assert!(!unknown.window_unknown());
        // An application's log covers its own period, which needn't end at the scan; one without timestamps, a period
        // nobody can say.
        let mut log = w(0.8, true);
        log.log = Some("Rails".into());
        assert_eq!(log.how_often(3.0), "3 runs over the 48 minutes the Rails log covers");
        log.window_hours = 0.0;
        assert!(log.window_unknown());
        assert_eq!(log.how_often(3.0), "3 runs over the period the Rails log covers");
        // Fewer runs than days: what they are, not "0.2 runs a day".
        assert_eq!(w(146.0, true).how_often(1.0), "1 run in the last 6 days");
    }

    fn statement(runs: f64, first: Option<&str>, last: Option<&str>) -> QueryStat {
        let mut q: QueryStat = serde_json::from_value(serde_json::json!({
            "id": "244", "query_hash": null, "text": "SELECT AVG(value) FROM ctx.readings WHERE sensor = @s",
            "parameters": null, "object_name": null, "executions": runs, "total_time_ms": runs * 5.0,
            "mean_time_ms": 5.0, "max_time_ms": null, "stddev_time_ms": null, "cpu_time_ms": null, "rows": null,
            "logical_reads": null, "physical_reads": null, "temp_writes": null, "first_seen": first,
            "last_seen": last, "plans": [], "plan": null, "waits": []
        }))
        .unwrap();
        q.trend = None;
        q
    }

    /// Six days of Query Store, Oct 11 06:00 to Oct 17 06:00.
    fn six_days() -> Workload {
        serde_json::from_value(serde_json::json!({
            "source": "Query Store", "kind": "query-store", "windowed": true,
            "window_start": "2026-10-11T06:00:00Z", "window_end": "2026-10-17T06:00:00Z", "window_hours": 144.0,
            "total_time_ms": 100000.0, "total_cpu_ms": null, "total_executions": 10000.0, "query_count": 1.0,
            "queries": []
        }))
        .unwrap()
    }

    // The fresh SQL Server demo: 750 runs in three minutes, then never again, read as "about 123 runs a day"; a single
    // run as "0.2 runs a day".
    #[test]
    fn a_burst_and_a_few_runs_are_said_as_they_came() {
        let w = six_days();
        let burst = statement(750.0, Some("2026-10-11T15:51:56Z"), Some("2026-10-11T15:55:15Z"));
        assert_eq!(w.how_often_of(&burst), "750 runs on Oct 11, 15:51–15:55 UTC");
        assert_eq!(w.runs_per_day_of(&burst), None);
        let once = statement(1.0, Some("2026-10-11T13:58:49Z"), Some("2026-10-11T13:58:49Z"));
        assert_eq!(w.how_often_of(&once), "1 run, on Oct 11 at 13:58 UTC");
        // One run known only to the trend buckets that hold it is a point between their bounds, not a 47-minute run.
        let bucketed = statement(1.0, Some("2026-10-11T08:41:00Z"), Some("2026-10-11T09:28:00Z"));
        assert_eq!(w.how_often_of(&bucketed), "1 run, sometime between 08:41 and 09:28 UTC on Oct 11");
        let night = statement(40.0, Some("2026-10-11T22:10:00Z"), Some("2026-10-12T03:10:00Z"));
        assert_eq!(w.how_often_of(&night), "40 runs between Oct 11, 22:10 and Oct 12, 03:10 UTC");
        // Three runs over the window, dates unknown: a count.
        assert_eq!(w.how_often_of(&statement(3.0, None, None)), "3 runs in the last 6 days");
        // Runs for two of the six days, then none: a rate over those two days.
        let stopped = statement(2000.0, Some("2026-10-11T08:00:00Z"), Some("2026-10-13T08:00:00Z"));
        assert_eq!(w.how_often_of(&stopped), "about 1.0K runs a day from Oct 11 to Oct 13");
        // Since two days ago, still running.
        let recent = statement(2000.0, Some("2026-10-15T06:00:00Z"), Some("2026-10-17T05:59:00Z"));
        assert_eq!(w.how_often_of(&recent), "about 1.0K runs a day since Oct 15");
        // Steady over the window.
        let steady = statement(6000.0, Some("2026-10-11T06:01:00Z"), Some("2026-10-17T05:59:00Z"));
        assert_eq!(w.how_often_of(&steady), "about 1.0K runs a day");
        assert_eq!(w.runs_per_day_of(&steady), Some(1000.0));
    }

    /// The fresh MySQL demo: performance_schema reset 1.66 hours before the scan, every statement run in the first
    /// minute (a temp-table spill of 35.8 s, 30K lookups in six seconds) and none since.
    fn two_hours_of_counters() -> Workload {
        serde_json::from_value(serde_json::json!({
            "source": "performance_schema", "kind": "performance-schema", "windowed": false,
            "window_start": "2026-10-17T10:17:02Z", "window_end": "2026-10-17T11:56:43Z", "window_hours": 1.66,
            "total_time_ms": 45784.0, "total_cpu_ms": null, "total_executions": 40080.0, "query_count": 6.0,
            "queries": []
        }))
        .unwrap()
    }

    // MY-1: a 40-second burst in a window under a day was read as runs a day ("433K a day" from 30K in six seconds)
    // and graded High on 623 s "a day" of spill.
    #[test]
    fn a_burst_in_a_window_under_a_day_is_when_it_ran_and_its_total() {
        let w = two_hours_of_counters();
        let burst = statement(30_000.0, Some("2026-10-17T10:17:02Z"), Some("2026-10-17T10:17:08Z"));
        assert_eq!(w.how_often_of(&burst), "30K runs on Oct 17 at 10:17 UTC");
        assert!(w.cadence(&burst).is_burst());
        // Graded on its total, whatever the window around it.
        assert_eq!(w.rate_per_day_of(&burst, 30_000.0), Some(30_000.0));
        assert_eq!(w.rate_per_day_of(&burst, 35_824.0), Some(35_824.0));
        // Runs from the window's start to its end are the window's: a rate over it.
        let steady = statement(30_000.0, Some("2026-10-17T10:17:02Z"), Some("2026-10-17T11:56:00Z"));
        assert!(!w.cadence(&steady).is_burst());
        assert_eq!(w.rate_per_day_of(&steady, 1.66), Some(24.0));
        // Under an hour of window there is no rate to judge: the total is all there is.
        let mut minutes = w.clone();
        minutes.window_hours = 0.03;
        assert_eq!(minutes.cadence(&burst), Cadence::Counted(30_000.0));
        assert_eq!(minutes.rate_per_day_of(&burst, 30_000.0), None);
    }

    /// Six days of Query Store in 2-hour buckets, Oct 11 06:00 to Oct 17 06:00.
    fn six_days_in_buckets() -> Workload {
        let mut w = six_days();
        let start = crate::format::epoch_seconds("2026-10-11T06:00:00Z").unwrap();
        let starts: Vec<String> = (0..72).map(|i| crate::format::iso_from_epoch(start + i as f64 * 7200.0)).collect();
        w.timeline = Some(Timeline {
            source: TimelineSource::QueryStore,
            time_ms: vec![1000.0; starts.len()],
            executions: vec![1000.0; starts.len()],
            starts,
            end: "2026-10-17T06:00:00Z".into(),
            unknown: None,
        });
        w
    }

    /// A statement whose runs fell in the buckets `ran` (index, runs) of [`six_days_in_buckets`].
    fn in_buckets(ran: &[(usize, f64)], first: &str, last: &str) -> QueryStat {
        let runs: f64 = ran.iter().map(|(_, r)| r).sum();
        let mut q = statement(runs, Some(first), Some(last));
        let mut executions = vec![Some(0.0); 72];
        for &(i, r) in ran {
            executions[i] = Some(r);
        }
        q.trend = Some(QueryTrend { time_ms: executions.clone(), executions });
        q
    }

    // MS-2 and MS-3: the planted N+1 ran 120K times in four 2-hour bursts on two of six days, from a first run on day
    // one to a last on day six: read as "about 19.4K runs a day", under chatty's bar, and its neighbours as "about 388
    // runs a day".
    #[test]
    fn runs_on_a_few_days_are_those_days_from_the_timelines_buckets() {
        let w = six_days_in_buckets();
        // Oct 11 06–08, 12–14, 14–16 and Oct 16 10–12.
        let n1 = in_buckets(
            &[(0, 30_000.0), (3, 30_000.0), (4, 30_000.0), (62, 30_000.0)],
            "2026-10-11T07:44:28Z",
            "2026-10-16T10:18:42Z",
        );
        assert_eq!(w.how_often_of(&n1), "120K runs on Oct 11 and Oct 16");
        assert_eq!(w.runs_per_day_of(&n1), None);
        // Busy at the pace it ran: 60K a day on the two days it ran, never 20K a day over six.
        assert_eq!(w.rate_per_day_of(&n1, 120_000.0), Some(60_000.0));
        // A few more days: named while they are three, counted past that.
        let three = in_buckets(&[(0, 10.0), (24, 10.0), (60, 10.0)], "2026-10-11T06:10:00Z", "2026-10-16T07:00:00Z");
        assert_eq!(w.how_often_of(&three), "30 runs on Oct 11, Oct 13 and Oct 16");
        // Every day of the window: a daily job, a rate a day.
        let nightly: Vec<(usize, f64)> = (0..6).map(|d| (d * 12, 100.0)).collect();
        let daily = in_buckets(&nightly, "2026-10-11T06:10:00Z", "2026-10-16T06:50:00Z");
        assert!(!w.cadence(&daily).is_burst(), "{:?}", w.cadence(&daily));
        // Buckets that don't hold all its runs (a timeline that began after it did) say nothing of when it ran.
        let mut partial = n1.clone();
        partial.executions = 200_000.0;
        assert!(!w.cadence(&partial).is_burst());
    }

    // MS-12c: "5.2 s per run" for one run of 10.5 s and one of 0.23 ms.
    #[test]
    fn one_run_or_two_far_apart_are_what_they_took() {
        let mut q = statement(2.0, None, None);
        q.total_time_ms = 10_490.35;
        q.max_time_ms = Some(10_490.125);
        assert_eq!(q.few_runs_took().as_deref(), Some("10.5 s and 0.23 ms"));
        q.max_time_ms = Some(5_300.0);
        assert_eq!(q.few_runs(), None, "two alike: the average says it");
        q.max_time_ms = None;
        assert_eq!(q.few_runs(), None, "no longest run kept: nothing to say apart");
        let mut once = statement(1.0, None, None);
        once.total_time_ms = 24_288.7;
        assert_eq!(once.few_runs_took().as_deref(), Some("24.3 s"));
        assert_eq!(statement(3.0, None, None).few_runs(), None);
    }

    /// pg_stat_statements counting since Jan 1, with 36 hours of Culprit's scans in three steps: the database ran
    /// 30 s in them, and the statement `runs` and `time_ms` in each.
    fn running_totals(runs: f64, time_ms: f64) -> (Workload, QueryStat) {
        let mut w: Workload = serde_json::from_value(serde_json::json!({
            "source": "pg_stat_statements", "kind": "statement-stats", "windowed": false,
            "window_start": "2026-01-01T00:00:00Z", "window_end": "2026-10-11T12:00:00Z", "window_hours": 6804.0,
            "total_time_ms": 1000000.0, "total_cpu_ms": null, "total_executions": 1000000.0, "query_count": 1.0,
            "queries": [],
            "timeline": {
                "source": "scans", "starts": ["2026-10-10T00:00:00Z", "2026-10-10T12:00:00Z", "2026-10-11T00:00:00Z"],
                "end": "2026-10-11T12:00:00Z", "time_ms": [10000.0, 10000.0, 10000.0],
                "executions": [10000.0, 10000.0, 10000.0]
            }
        }))
        .unwrap();
        w.window_hours = (crate::format::epoch_seconds("2026-10-11T12:00:00Z").unwrap()
            - crate::format::epoch_seconds("2026-01-01T00:00:00Z").unwrap())
            / 3600.0;
        // 283.5 days at 388 runs a day, a third of all the time.
        let mut q = statement(110_000.0, None, None);
        q.total_time_ms = 333_333.0;
        q.trend = Some(QueryTrend { time_ms: vec![Some(time_ms); 3], executions: vec![Some(runs); 3] });
        (w, q)
    }

    // The aptabase readings: "about 388 times a day" averaged over three and a half years, where Culprit's own scans
    // measured 1,670 a day. Within twice the average, the scans' rate is the one said; past it, both.
    #[test]
    fn a_rate_on_running_totals_is_what_culprits_scans_measured() {
        let (w, q) = running_totals(800.0, 3000.0);
        let s = w.over_scans(&q).expect("36 hours of scans");
        assert_eq!((s.hours, s.runs, s.time_ms, s.workload_time_ms), (36.0, 2400.0, 9000.0, 30000.0));
        assert_eq!(s.length(), "36 hours");
        assert_eq!(
            w.how_often_of(&q),
            "about 1.6K runs a day over 36 hours of Culprit's scans (about 388 a day over the 284 days since statistics were reset)"
        );
        // About the same pace and share as since the reset: the scans' rate alone.
        let (w, q) = running_totals(200.0, 4000.0);
        assert_eq!(w.how_often_of(&q), "about 400 runs a day over 36 hours of Culprit's scans");
        // A step whose totals are unknown, or where the statement outran the database (a misread counter), is left
        // out; under a day of steps, nothing is said of them.
        let (mut w, q) = running_totals(800.0, 3000.0);
        w.timeline.as_mut().unwrap().unknown = Some(vec![false, true, false]);
        assert_eq!(w.over_scans(&q).map(|s| s.hours), Some(24.0));
        w.timeline.as_mut().unwrap().time_ms[0] = 100.0;
        assert_eq!(w.over_scans(&q), None);
        // A windowed source's own timeline is no scans of Culprit's.
        let (mut w, q) = running_totals(800.0, 3000.0);
        w.windowed = true;
        assert_eq!(w.over_scans(&q), None);
    }

    // The aptabase readings again: a read titled "19% of database time" and ranked below DISCARD ALL took 33% over
    // Culprit's scans, and a read titled 6% took 3.1%. Over a day of scans, share, runs, average and time a day are the
    // scans', in both directions; the totals since the reset are said beside them where they differ twofold.
    #[test]
    fn a_statement_on_running_totals_is_read_by_culprits_scans_both_ways() {
        // 9 s of the database's 30 s in the scans (30%) at 3.75 ms a run, about the totals' third at 5 ms, but four
        // times as many runs a day.
        let (w, q) = running_totals(800.0, 3000.0);
        assert!((w.statement_share(&q) - 0.3).abs() < 1e-9);
        assert_eq!(w.statement_mean_ms(&q), 3.75);
        assert_eq!(w.statement_time_per_day(&q), Some(6000.0));
        let cost = w.cost_of(&q);
        assert_eq!(cost.runs, "about 1.6K runs a day over 36 hours of Culprit's scans");
        // The share (30% against 33%) is within twice: only the runs a day differ.
        assert_eq!(cost.totals.as_deref(), Some("about 388 runs a day over the 284 days since statistics were reset"));
        // Lower now: 2.4 s of 30 s (8%), against a third of the totals.
        let (w, q) = running_totals(200.0, 800.0);
        assert!((w.statement_share(&q) - 0.08).abs() < 1e-9);
        assert_eq!(
            w.cost_of(&q).totals.as_deref(),
            Some("33% of database time over the 284 days since statistics were reset")
        );
        // A statement that didn't run in the scans is read by its totals: what it took until it stopped.
        let (w, q) = running_totals(0.0, 0.0);
        assert_eq!(w.scans_of(&q), None);
        assert!((w.statement_share(&q) - w.share_of(q.total_time_ms, &q)).abs() < 1e-9);
        assert_eq!(w.statement_mean_ms(&q), q.mean_time_ms);
        // A recent entry: most of its runs came in the scans, so its totals say nothing beside them.
        let (w, mut q) = running_totals(800.0, 3000.0);
        q.executions = 3000.0;
        assert!(w.scans_of(&q).unwrap().is_recent(&q));
        assert_eq!(w.cost_of(&q).totals, None);
        assert_eq!(w.how_often_of(&q), "about 1.6K runs a day over 36 hours of Culprit's scans");
    }

    use super::*;

    #[test]
    fn server_versions_compare_by_number() {
        assert!(!version_at_least("5.7.44", [8, 0, 18]));
        assert!(!version_at_least("8.0.17-log", [8, 0, 18]));
        assert!(version_at_least("8.0.18", [8, 0, 18]));
        assert!(version_at_least("8.4.3", [8, 0, 18]));
        assert!(version_at_least("10.5.29-MariaDB-ubu2004", [10, 5, 0]));
        assert!(!version_at_least("10.5.29-MariaDB-ubu2004", [10, 6, 0]));
        assert!(version_at_least("8.0", [8, 0, 0]));
        // Unknown builds are never refused for their age.
        assert!(version_at_least("", [8, 0, 18]));
        assert!(version_at_least("unknown", [8, 0, 18]));
    }
}
