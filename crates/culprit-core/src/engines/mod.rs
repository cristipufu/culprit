//! Everything Culprit knows about each engine, one file per engine: its name, what it can answer, how it spells the SQL
//! Culprit suggests ([`Dialect`]), and how it says what every check needs to say about it (applying a setting,
//! refreshing statistics, what its statement statistics don't record). Checks ask the engine's [`EngineSpec`] instead of
//! matching the engine. A new engine is one more file here and one arm in [`Engine::spec`]; advice a single check gives
//! for one engine (how to fix a spill on PostgreSQL) stays with that check.

mod mssql;
mod mysql;
mod postgres;
mod sqlite;
mod timescale;
mod yugabyte;

pub use yugabyte::session_target as ysql_session_target;

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::dialect::Dialect;
use crate::error::{Error, ErrorCode};
use crate::finding::Fix;
use crate::model::{Ability, Engine, EngineAbilities, ServerInfo, SessionAction, wire_name};

/// How to apply a server setting: the SQL that does it (`sp_configure`, `ALTER SYSTEM` and a reload, `SET PERSIST`,
/// `PRAGMA`) and a sentence on where and how. A managed service changes settings in its own control instead; the checks'
/// `setting_change` says so before asking the engine.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingChange {
    /// One sentence: where and how to set it.
    pub how: String,
    /// The statements, on servers where they work.
    pub sql: Option<String>,
}

/// One engine, as checks and every surface see it.
pub trait EngineSpec: Send + Sync {
    fn sql_syntax(&self) -> SqlSyntax;
    /// Meaning and presentation of statement counters, independently of SQL grammar or storage.
    fn statement_metrics(&self) -> StatementMetrics;
    /// How a live wait sample is collected. Required so a new engine cannot inherit another engine's description.
    fn wait_sampling(&self) -> Option<WaitSampling>;
    fn mutating_function(&self, _name: &str) -> bool {
        false
    }
    fn inactive_index_state(&self, server: &ServerInfo) -> crate::IndexState;
    fn limitations(&self) -> Vec<String>;
    /// Public collection limits, safe to show after private collector details have been removed for sharing.
    fn deadlock_limitation(&self) -> Option<&'static str> {
        None
    }
    /// "SQL Server": the engine's name in sentences (a server's product can say more: MariaDB).
    fn label(&self) -> &'static str;

    /// What it can answer: sessions, wait samples, measured runs… (see [`EngineAbilities`]).
    fn abilities(&self) -> EngineAbilities;

    /// How it spells the SQL Culprit suggests.
    fn dialect(&self) -> &'static dyn Dialect;

    /// A storage-managed table may constrain DDL even when it speaks the server's usual SQL dialect.
    fn table_dialect(&self, _kind: Option<&str>) -> &'static dyn Dialect {
        self.dialect()
    }

    /// How to set `name` to `value` on a server Culprit reaches with SQL.
    fn change_setting(&self, server: &ServerInfo, name: &str, value: &str) -> SettingChange;

    /// How to set `name` to `value` for the scanned database alone, where the engine keeps settings per database and
    /// its owner may set this one without being a superuser (PostgreSQL's `ALTER DATABASE … SET` for a setting of
    /// context `user`). It works on a managed service too, where the server's own settings are the platform's. `None`
    /// where the setting is only the server's.
    fn database_setting(&self, _server: &ServerInfo, _name: &str, _value: &str) -> Option<SettingChange> {
        None
    }

    /// The fix for a table whose optimizer statistics are stale: recompute them (`target` already qualified), and how
    /// to keep them fresh.
    fn refresh_statistics(&self, target: &str) -> Fix;

    /// Why a wait in `category` ("io", "lock", "cpu"…) isn't put on any statement.
    fn wait_not_per_statement(&self, server: &ServerInfo, category: &str) -> String;

    /// Where the index advisor's candidates come from here, when that needs saying (no missing-index requests, no
    /// statement history). `has_workload`: the scan has statement statistics.
    fn index_advice_sources(&self, server: &ServerInfo, has_workload: bool) -> Option<String>;

    /// How a person connects to it, as the new-connection form asks.
    fn connect(&self) -> Connect;

    /// What may be done to another session from Live, outside production.
    fn session_actions(&self) -> Vec<SessionAction> {
        Vec::new()
    }

    /// Validate a Live session's engine-specific identity before resolving credentials or issuing an operation.
    fn validate_session_id(&self, session: &str) -> crate::Result<()> {
        crate::session_number(session).map(|_| ())
    }

    /// The word the engine uses for a statement that is waiting ("suspended" on SQL Server).
    fn waiting_state(&self) -> &'static str {
        "active"
    }

    /// Why it can't do `ability`, said the same by the desktop, the CLI, the MCP server and the browser demo. An engine
    /// overrides the ones it can say better than this.
    fn refusal(&self, ability: Ability) -> Refusal {
        Refusal {
            message: format!("{} can't do this ({})", self.label(), wire_name(&ability).replace('_', " ")),
            hint: None,
        }
    }
}

/// The SQL grammar a server speaks, independently of its storage engine and measurements.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SqlSyntax {
    Mssql,
    Postgres,
    Mysql,
    Sqlite,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum WaitSampling {
    Sessions,
    Counters,
}

/// Counter semantics shared with the app. Optional facts describe support; a particular scan may still lack data.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct StatementMetrics {
    pub planning_label: Cow<'static, str>,
    pub planning_setting: Option<Cow<'static, str>>,
    pub compilation_history: bool,
    pub write_rows_label: Option<Cow<'static, str>>,
    pub lock_time: bool,
    pub log_name: Cow<'static, str>,
    pub log_label: Cow<'static, str>,
    /// `{engine}` is replaced by the server's display name, including product variants such as MariaDB.
    pub missing_costs_note: Option<Cow<'static, str>>,
    pub errors_note: Option<Cow<'static, str>>,
}

impl StatementMetrics {
    const BASIC: Self = Self {
        planning_label: Cow::Borrowed("Planning"),
        planning_setting: None,
        compilation_history: false,
        write_rows_label: Some(Cow::Borrowed("Rows returned per run")),
        lock_time: false,
        log_name: Cow::Borrowed("log"),
        log_label: Cow::Borrowed("Log written"),
        missing_costs_note: None,
        errors_note: None,
    };

    /// YSQL and PostgreSQL share these statement statistics, without sharing heap or WAL measurements.
    const PG_STAT_STATEMENTS: Self = Self {
        planning_setting: Some(Cow::Borrowed("pg_stat_statements.track_planning")),
        write_rows_label: Some(Cow::Borrowed("Rows changed per run")),
        missing_costs_note: Some(Cow::Borrowed(
            "{engine} keeps no error count or lock time per statement; Live shows blocking as it happens.",
        )),
        ..Self::BASIC
    };
}

/// Why an engine can't do something: the message and, when there's one, what to do instead.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Refusal {
    pub message: String,
    pub hint: Option<String>,
}

impl Refusal {
    fn new(message: &str, hint: Option<&str>) -> Refusal {
        Refusal { message: message.into(), hint: hint.map(str::to_string) }
    }
}

/// How a person connects to an engine: the file they choose, or what they paste and how Culprit reads it.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Connect {
    /// The engine where a person picks it, with what else it serves: "MySQL / MariaDB".
    pub choice: String,
    /// The database is a file on this computer (SQLite), chosen rather than pasted.
    pub file: bool,
    /// What to paste, or the path to type, as an example.
    pub example: String,
    /// What Culprit reads, in a sentence.
    pub accepts: String,
}

impl Connect {
    fn server(choice: &str, example: &str, accepts: &str) -> Connect {
        Connect { choice: choice.into(), file: false, example: example.into(), accepts: accepts.into() }
    }
}

/// Everything the app needs to know about an engine without a connection: written to `app/src/gen/engines.json`.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct EngineFacts {
    pub label: String,
    pub abilities: EngineAbilities,
    pub session_actions: Vec<SessionAction>,
    pub waiting_state: String,
    pub connect: Connect,
    pub limitations: Vec<String>,
    /// The reason for every ability it lacks, by ability.
    pub refusals: BTreeMap<String, Refusal>,
    pub sql_syntax: SqlSyntax,
    pub statement_metrics: StatementMetrics,
    pub wait_sampling: Option<WaitSampling>,
}

impl Engine {
    /// Everything Culprit knows about this engine.
    pub fn spec(self) -> &'static dyn EngineSpec {
        match self {
            Engine::Mssql => &mssql::Mssql,
            Engine::Postgres => &postgres::Postgres,
            Engine::Yugabyte => &yugabyte::Yugabyte,
            Engine::Mysql => &mysql::Mysql,
            Engine::Sqlite => &sqlite::Sqlite,
        }
    }

    pub fn label(self) -> &'static str {
        self.spec().label()
    }

    pub fn sql_syntax(self) -> SqlSyntax {
        self.spec().sql_syntax()
    }

    /// What the engine can answer, defined in its own file: the CLI, the MCP server and the app offer only these, and
    /// the app reads this same table (`app/src/gen/abilities.json`, written by this crate's tests).
    pub fn abilities(self) -> EngineAbilities {
        self.spec().abilities()
    }

    /// How this engine spells the SQL Culprit suggests.
    pub fn dialect(self) -> &'static dyn Dialect {
        self.spec().dialect()
    }

    /// The error a surface returns when asked for something the engine can't do.
    pub fn refused(self, ability: Ability) -> Error {
        let r = self.spec().refusal(ability);
        let error = Error::new(ErrorCode::Unsupported, r.message);
        match r.hint {
            Some(hint) => error.with_hint(hint),
            None => error,
        }
    }

    /// What the app reads about this engine (see [`EngineFacts`]).
    pub fn facts(self) -> EngineFacts {
        let spec = self.spec();
        let abilities = spec.abilities();
        EngineFacts {
            statement_metrics: spec.statement_metrics(),
            wait_sampling: spec.wait_sampling(),
            sql_syntax: spec.sql_syntax(),
            label: spec.label().into(),
            abilities,
            session_actions: spec.session_actions(),
            waiting_state: spec.waiting_state().into(),
            connect: spec.connect(),
            limitations: spec.limitations(),
            refusals: Ability::ALL
                .into_iter()
                .filter(|a| !abilities.has(*a))
                .map(|a| (wire_name(&a), spec.refusal(a)))
                .collect(),
        }
    }
}

/// A setting's value as a SQL literal: a number as it is, anything else quoted.
fn literal(value: &str) -> String {
    if value.chars().all(|c| c.is_ascii_digit() || c == '.') { value.to_string() } else { format!("'{value}'") }
}

/// A server engine Culprit talks to: everything but what only one engine keeps (SQL Server's last actual plans and live
/// per-step counters), which that engine adds.
const SERVER: EngineAbilities = EngineAbilities {
    sessions: true,
    wait_sample: true,
    deadlocks: true,
    measures_runs: true,
    plan_estimates: true,
    writes: true,
    periods: true,
    last_actual_plan: false,
    operator_progress: false,
    sign_in: true,
};
