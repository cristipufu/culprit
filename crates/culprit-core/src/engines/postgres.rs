//! PostgreSQL, including the managed services (RDS, Aurora, Azure, Cloud SQL).

use super::*;
use crate::dialect::{NewIndex, included, keys, unique};

pub(super) struct Postgres;

pub(super) fn session_reset_advice(command: &str) -> Option<&'static str> {
    matches!(command, "discard" | "reset").then_some("This resets session state and is commonly sent by a driver or connection pool; the statement statistics do not identify its caller or pooling mode. Inspect the application's connection lifecycle and the driver or pool configuration, and benchmark any change against the measured absolute cost shown here. Keep resets that prevent session state leaking between users. Npgsql arranges its reset on close and sends it on reuse; PgBouncer normally resets on release in session pooling, and normally does not in transaction pooling. Consider changing reset behavior only after verifying that every caller safely handles retained session state.")
}

/// Settings of context `user` in pg_settings: a database's owner sets them for that database (`ALTER DATABASE … SET`),
/// no superuser needed, so they work on a managed service too. Settings of context `superuser` (track_io_timing,
/// pg_stat_statements.track, wal_compression) are left out: a managed service's admin role isn't a superuser.
const USER_SETTINGS: &[&str] = &[
    "random_page_cost",
    "seq_page_cost",
    "cpu_tuple_cost",
    "cpu_index_tuple_cost",
    "cpu_operator_cost",
    "parallel_setup_cost",
    "parallel_tuple_cost",
    "min_parallel_table_scan_size",
    "min_parallel_index_scan_size",
    "effective_cache_size",
    "effective_io_concurrency",
    "work_mem",
    "maintenance_work_mem",
    "hash_mem_multiplier",
    "temp_buffers",
    "max_parallel_workers_per_gather",
    "jit",
    "jit_above_cost",
    "jit_inline_above_cost",
    "jit_optimize_above_cost",
    "default_statistics_target",
    "from_collapse_limit",
    "join_collapse_limit",
    "plan_cache_mode",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
];

impl EngineSpec for Postgres {
    fn unused_index_advice(&self, _server: &ServerInfo, table_kind: Option<&str>) -> &'static str {
        if matches!(table_kind, Some("hypertable" | "timescale-chunk" | "timescale-unresolved")) {
            "Save the definition first so you can recreate it. TimescaleDB does not support dropping hypertable indexes concurrently; schedule the drop for a maintenance window."
        } else {
            "Drop it concurrently so writes aren't blocked. Save the definition first so you can recreate it."
        }
    }
    fn session_advice(&self, command: &str) -> Option<&'static str> {
        session_reset_advice(command)
    }
    fn statement_metrics(&self) -> StatementMetrics {
        StatementMetrics {
            log_name: "WAL".into(),
            log_label: "WAL written".into(),
            ..StatementMetrics::PG_STAT_STATEMENTS
        }
    }
    fn wait_sampling(&self) -> Option<WaitSampling> {
        Some(WaitSampling::Sessions)
    }
    fn sql_syntax(&self) -> SqlSyntax {
        SqlSyntax::Postgres
    }
    fn deadlock_limitation(&self) -> Option<&'static str> {
        Some(
            "PostgreSQL keeps a cumulative deadlock count in SQL, but the graphs and statements are only in the server log. Import the relevant log entries to explain the pattern. Turning on log_lock_waits also logs waits longer than deadlock_timeout.",
        )
    }
    fn inactive_index_state(&self, _server: &ServerInfo) -> crate::IndexState {
        crate::IndexState::Invalid
    }
    fn limitations(&self) -> Vec<String> {
        vec![
        "pg_stat_statements keeps aggregate totals, not per-request traces, historical parameter values or the plan used by each execution. Culprit's scans provide comparisons only over their recorded intervals.".into(),
        "A newly obtained generic plan uses this connection's role, settings and search path; it may differ from the application's custom plan. Actual rows, spills and timing require a measured plan.".into(),
        "Deadlock graphs, statement errors and auto_explain output live in server logs. SQL counters alone cannot reconstruct them; import the relevant logs or correlate application telemetry.".into(),
    ]
    }
    fn label(&self) -> &'static str {
        "PostgreSQL"
    }

    fn connect(&self) -> Connect {
        Connect::server(
            "PostgreSQL",
            "postgres://app@db.example.com:5432/shop",
            "A postgres:// or JDBC URL, libpq's key=value pairs, an Npgsql connection string, or host:port/database.",
        )
    }

    fn abilities(&self) -> EngineAbilities {
        SERVER
    }

    fn session_actions(&self) -> Vec<SessionAction> {
        vec![SessionAction::Cancel, SessionAction::Terminate]
    }

    fn refusal(&self, ability: Ability) -> Refusal {
        match ability {
            Ability::LastActualPlan => Refusal::new(
                "PostgreSQL doesn't keep the actual plan of past runs",
                Some(
                    "auto_explain can write them to the server log, which Culprit can't read over SQL. Open the query in the \
                     editor with real values and use Analyze: it runs inside a transaction that is rolled back.",
                ),
            ),
            Ability::OperatorProgress => Refusal::new(
                "PostgreSQL reports a running statement's progress, not each step's",
                Some("Its plan doesn't change while it runs: ask for the plan once."),
            ),
            _ => Refusal::new(&format!("PostgreSQL can't do this ({})", wire_name(&ability).replace('_', " ")), None),
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &Postgres
    }

    fn table_dialect(&self, kind: Option<&str>) -> &'static dyn Dialect {
        match kind {
            Some("hypertable" | "timescale-chunk") => &super::timescale::Timescale { per_chunk: true },
            Some("timescale-unresolved") => &super::timescale::Timescale { per_chunk: false },
            _ => self.dialect(),
        }
    }

    fn change_setting(&self, _server: &ServerInfo, name: &str, value: &str) -> SettingChange {
        SettingChange {
            how: format!(
                "Set {name} to {value} and reload the configuration (settings marked as needing a restart take effect after one)."
            ),
            sql: Some(format!("ALTER SYSTEM SET {name} = {};\nSELECT pg_reload_conf();", literal(value))),
        }
    }

    fn database_setting(&self, server: &ServerInfo, name: &str, value: &str) -> Option<SettingChange> {
        if !USER_SETTINGS.iter().any(|s| s.eq_ignore_ascii_case(name)) {
            return None;
        }
        Some(SettingChange {
            how: format!(
                "Set {name} to {value} for this database only, as its owner: sessions that connect after the change use \
                 it, and other databases on the server keep theirs."
            ),
            sql: Some(format!(
                "ALTER DATABASE {} SET {name} = {};",
                Dialect::quote(self, &server.database),
                literal(value)
            )),
        })
    }

    fn refresh_statistics(&self, target: &str) -> Fix {
        Fix {
            summary: "Analyze the table now. If autovacuum falls behind on it regularly, lower its analyze threshold for this table only.".into(),
            sql: Some(format!(
                "{}\nALTER TABLE {target} SET (autovacuum_analyze_scale_factor = 0.02);",
                self.dialect().refresh_statistics(target)
            )),
            cost: Some("ANALYZE reads a sample of the table and does not block reads or writes.".into()),
        }
    }

    fn wait_not_per_statement(&self, _server: &ServerInfo, category: &str) -> String {
        match category {
            "io" => "No statement recorded disk-read time.".into(),
            _ => "PostgreSQL doesn't time this per statement. Right now samples what sessions wait on.".into(),
        }
    }

    fn index_advice_sources(&self, _server: &ServerInfo, _has_workload: bool) -> Option<String> {
        Some(
            "PostgreSQL doesn't record missing-index requests. Candidates come from captured plans that read whole \
             tables, foreign keys, and the table scan counters."
                .into(),
        )
    }
}

impl Dialect for Postgres {
    fn quote(&self, ident: &str) -> String {
        let simple = ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !ident.starts_with(|c: char| c.is_ascii_digit());
        if simple { ident.to_string() } else { format!("\"{}\"", ident.replace('"', "\"\"")) }
    }

    fn create_index(&self, index: &NewIndex) -> String {
        let mut sql = format!(
            "CREATE {}INDEX CONCURRENTLY {}\n    ON {} ({})",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        );
        let include = included(self, index);
        if !include.is_empty() {
            sql.push_str(&format!("\n    INCLUDE ({})", include.join(", ")));
        }
        if let Some(filter) = index.filter {
            sql.push_str(&format!("\n    WHERE {filter}"));
        }
        sql.push(';');
        sql
    }

    fn drop_index(&self, schema: &str, _table: &str, index: &str) -> String {
        format!("DROP INDEX CONCURRENTLY {};", self.qualified(&[schema, index]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE {target};")
    }

    fn end_session(&self, session: &str) -> String {
        format!("SELECT pg_terminate_backend({session});")
    }
}
