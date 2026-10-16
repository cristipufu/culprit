//! SQLite: a file the application opens itself, which Culprit opens read-only. No server to hold sessions, waits or
//! deadlocks; EXPLAIN QUERY PLAN has the access path only; a query log is one period, with none to compare.

use super::*;
use crate::dialect::{NewIndex, keys, unique};

pub(super) struct Sqlite;

impl EngineSpec for Sqlite {
    fn label(&self) -> &'static str {
        "SQLite"
    }

    fn abilities(&self) -> EngineAbilities {
        EngineAbilities {
            sessions: false,
            wait_sample: false,
            deadlocks: false,
            measures_runs: false,
            plan_estimates: false,
            writes: false,
            periods: false,
            last_actual_plan: false,
            operator_progress: false,
            sign_in: false,
        }
    }

    fn refusal(&self, ability: Ability) -> Refusal {
        let busy = "A writer that has to wait gets SQLITE_BUSY in the application: its logs say when.";
        match ability {
            Ability::Sessions | Ability::OperatorProgress => {
                Refusal::new("SQLite has no server, so no sessions to show", Some(busy))
            }
            Ability::Deadlocks => Refusal::new(
                "SQLite records no deadlocks: one writer at a time holds the file",
                Some(
                    "A transaction that can't get the write lock waits busy_timeout, then gets SQLITE_BUSY in the application: its logs say when.",
                ),
            ),
            Ability::WaitSample => Refusal::new(
                "SQLite has no server, so nothing records what it waits on",
                Some(
                    "Time spent waiting for the write lock shows up in the application as SQLITE_BUSY or slow commits.",
                ),
            ),
            Ability::MeasuresRuns | Ability::LastActualPlan | Ability::PlanEstimates => Refusal::new(
                "SQLite's plans have no actual rows or timings: EXPLAIN QUERY PLAN shows the access path only",
                Some("Use the estimated plan; time the statement in the application to see how long it takes."),
            ),
            Ability::Writes => Refusal::new(
                "Culprit opens SQLite files read-only, so it never changes one",
                Some("Run it with the sqlite3 shell or from the application, then scan again to see the effect."),
            ),
            Ability::Periods => Refusal::new(
                "SQLite keeps no statement history to compare two periods of",
                Some(
                    "Add the log your framework writes to the connection, scan before and after the change, and compare the two reports' queries.",
                ),
            ),
            Ability::SignIn => {
                Refusal::new("A SQLite file takes no sign-in: Culprit opens it read-only from disk", None)
            }
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &Sqlite
    }

    // A pragma that lives in the file (journal_mode = WAL) is set once; one that lives in the connection (synchronous,
    // cache_size) has to be set by the application each time it opens the file.
    fn change_setting(&self, _server: &ServerInfo, name: &str, value: &str) -> SettingChange {
        SettingChange {
            how: format!(
                "Set {name} to {value} with PRAGMA. If it is a per-connection pragma, the application has to set it each time it opens the file."
            ),
            sql: Some(format!("PRAGMA {name} = {value};")),
        }
    }

    fn refresh_statistics(&self, target: &str) -> Fix {
        Fix {
            summary: "Run ANALYZE, so the planner knows each table's size and how selective each index is.".into(),
            sql: Some(self.dialect().refresh_statistics(target)),
            cost: Some("ANALYZE reads the indexes and briefly takes the write lock to store what it found.".into()),
        }
    }

    fn wait_not_per_statement(&self, _server: &ServerInfo, _category: &str) -> String {
        "SQLite keeps no waits.".into()
    }

    fn index_advice_sources(&self, _server: &ServerInfo, has_workload: bool) -> Option<String> {
        Some(if has_workload {
            "SQLite doesn't record missing-index requests. Candidates come from the indexes it builds for itself on \
             every run of the logged statements, and foreign keys."
                .into()
        } else {
            "SQLite keeps no history of the statements it ran, so candidates come from foreign keys only. With the \
             application's query log on the connection, the indexes SQLite builds for itself on every run join them."
                .into()
        })
    }
}

impl Dialect for Sqlite {
    fn quote(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn create_index(&self, index: &NewIndex) -> String {
        // No INCLUDE and no online build: SQLite builds an index under the database's write lock, so writers wait.
        let mut sql = format!(
            "CREATE {}INDEX IF NOT EXISTS {}\n    ON {} ({})",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        );
        if let Some(filter) = index.filter {
            sql.push_str(&format!("\n    WHERE {filter}"));
        }
        sql.push_str(";\n-- SQLite builds an index under the database's write lock: writers wait until it finishes.");
        sql
    }

    fn drop_index(&self, schema: &str, _table: &str, index: &str) -> String {
        format!("DROP INDEX IF EXISTS {};", self.qualified(&[schema, index]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE {target};")
    }

    fn end_session(&self, _session: &str) -> String {
        "-- SQLite has no sessions to end: the application that holds the write lock has to finish its transaction."
            .into()
    }
}
