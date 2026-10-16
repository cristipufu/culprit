//! Everything Culprit knows about each engine, one file per engine: its name, what it can answer, how it spells the SQL
//! Culprit suggests ([`Dialect`]), and how it says what every check needs to say about it (applying a setting,
//! refreshing statistics, what its statement statistics don't record). Checks ask the engine's [`EngineSpec`] instead of
//! matching the engine. A new engine is one more file here and one arm in [`Engine::spec`]; advice a single check gives
//! for one engine (how to fix a spill on PostgreSQL) stays with that check.

mod mssql;
mod mysql;
mod postgres;
mod sqlite;

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
    /// "SQL Server": the engine's name in sentences (a server's product can say more: MariaDB).
    fn label(&self) -> &'static str;

    /// What it can answer: sessions, wait samples, measured runs… (see [`EngineAbilities`]).
    fn abilities(&self) -> EngineAbilities;

    /// How it spells the SQL Culprit suggests.
    fn dialect(&self) -> &'static dyn Dialect;

    /// How to set `name` to `value` on a server Culprit reaches with SQL.
    fn change_setting(&self, server: &ServerInfo, name: &str, value: &str) -> SettingChange;

    /// The fix for a table whose optimizer statistics are stale: recompute them (`target` already qualified), and how
    /// to keep them fresh.
    fn refresh_statistics(&self, target: &str) -> Fix;

    /// Why a wait in `category` ("io", "lock", "cpu"…) isn't put on any statement.
    fn wait_not_per_statement(&self, server: &ServerInfo, category: &str) -> String;

    /// Where the index advisor's candidates come from here, when that needs saying (no missing-index requests, no
    /// statement history). `has_workload`: the scan has statement statistics.
    fn index_advice_sources(&self, server: &ServerInfo, has_workload: bool) -> Option<String>;

    /// What may be done to another session from Live, outside production.
    fn session_actions(&self) -> Vec<SessionAction> {
        Vec::new()
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

/// Everything the app needs to know about an engine without a connection: written to `app/src/gen/engines.json`.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct EngineFacts {
    pub label: String,
    pub abilities: EngineAbilities,
    pub session_actions: Vec<SessionAction>,
    pub waiting_state: String,
    /// The reason for every ability it lacks, by ability.
    pub refusals: BTreeMap<String, Refusal>,
}

impl Engine {
    /// Everything Culprit knows about this engine.
    pub fn spec(self) -> &'static dyn EngineSpec {
        match self {
            Engine::Mssql => &mssql::Mssql,
            Engine::Postgres => &postgres::Postgres,
            Engine::Mysql => &mysql::Mysql,
            Engine::Sqlite => &sqlite::Sqlite,
        }
    }

    pub fn label(self) -> &'static str {
        self.spec().label()
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
            label: spec.label().into(),
            abilities,
            session_actions: spec.session_actions(),
            waiting_state: spec.waiting_state().into(),
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
