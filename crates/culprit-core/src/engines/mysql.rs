//! MySQL and MariaDB, told apart by the server's product where they differ.

use super::*;
use crate::dialect::{NewIndex, keys, unique};
use crate::model::Capability;

pub(super) struct Mysql;

fn version_pair(server: &ServerInfo) -> Option<(u32, u32)> {
    let mut parts = server.version.split(['.', '-']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn index_hiding(server: &ServerInfo) -> Option<&'static str> {
    let version = version_pair(server)?;
    if server.is_mariadb() {
        (version >= (10, 6)).then_some("IGNORED")
    } else {
        (version >= (8, 0)).then_some("INVISIBLE")
    }
}

impl EngineSpec for Mysql {
    fn unused_index_advice(&self, server: &ServerInfo, _table_kind: Option<&str>) -> &'static str {
        if version_pair(server).is_none() {
            "Save its definition before dropping it. Index-hiding support is not established for this server version."
        } else if index_hiding(server) == Some("IGNORED") {
            "Have the optimizer ignore it first: it stays maintained, and `ALTER INDEX … NOT IGNORED` brings it back instantly. Drop it after a full business cycle without complaints."
        } else if server.is_mariadb() {
            "Save its definition, then drop it: this MariaDB version can't hide an index (ignored indexes arrived in 10.6)."
        } else if index_hiding(server).is_none() {
            "Save its definition, then drop it: MySQL 5.7 can't hide an index (invisible indexes arrived in 8.0)."
        } else {
            "Make it invisible first: the optimizer stops using it but it stays maintained, and `ALTER INDEX … VISIBLE` brings it back instantly. Drop it after a full business cycle without complaints."
        }
    }
    fn hide_index(&self, server: &ServerInfo, schema: &str, table: &str, index: &str) -> Option<String> {
        let state = index_hiding(server)?;
        Some(format!("ALTER TABLE {} ALTER INDEX {} {state};", self.qualified(&[schema, table]), self.quote(index)))
    }
    fn statement_metrics(&self) -> StatementMetrics {
        StatementMetrics {
            write_rows_label: None,
            lock_time: true,
            runtime_note: "This {engine} scan includes no run-time histogram; the lane shows fastest, average and slowest. Histogram availability depends on the product, server version and collection permissions; the slow query log can add individual logged runs.".into(),
            runtime_histogram_note: Some("{engine} keeps a run-time histogram per statement, so the bars show how many runs took how long.".into()),
            missing_costs_note: Some("{engine} keeps no planning time or redo bytes per statement.".into()),
            ..StatementMetrics::BASIC
        }
    }
    fn wait_sampling(&self) -> Option<WaitSampling> {
        Some(WaitSampling::Counters)
    }
    fn sql_syntax(&self) -> SqlSyntax {
        SqlSyntax::Mysql
    }
    fn deadlock_limitation(&self) -> Option<&'static str> {
        Some(
            "InnoDB keeps only its most recent deadlock. Turn on innodb_print_all_deadlocks to write every one to the error log.",
        )
    }
    fn inactive_index_state(&self, server: &ServerInfo) -> crate::IndexState {
        if server.is_mariadb() { crate::IndexState::Ignored } else { crate::IndexState::Invisible }
    }
    fn limitations(&self) -> Vec<String> {
        vec![
        "Statement digests keep aggregate totals, not an application's request trace, transaction boundaries or the plan of every historical execution. Culprit compares only intervals its scans or the slow log record.".into(),
        "InnoDB status keeps only the latest deadlock. A complete incident history requires the error log; an empty or old latest report does not prove that no other deadlocks happened.".into(),
        "MySQL and MariaDB differ in plan details, instrumentation and syntax. Runtime measurements, metadata-lock visibility and replication diagnostics depend on the product, version and permissions.".into(),
    ]
    }
    fn label(&self) -> &'static str {
        "MySQL"
    }

    fn connect(&self) -> Connect {
        Connect::server(
            "MySQL / MariaDB",
            "mysql://app@db.example.com:3306/shop",
            "A mysql://, mariadb:// or JDBC URL, a .NET connection string (Server=…;Uid=…), or host:port/database.",
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
                "MySQL and MariaDB don't keep the actual plan of past runs",
                Some(
                    "Measure it instead: Analyze runs the statement's recorded text once inside a read-only transaction.",
                ),
            ),
            Ability::OperatorProgress => Refusal::new(
                "MySQL reports a running statement's progress, not each step's",
                Some("Its plan doesn't change while it runs: ask for the plan once."),
            ),
            _ => Refusal::new(&format!("MySQL can't do this ({})", wire_name(&ability).replace('_', " ")), None),
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &Mysql
    }

    fn change_setting(&self, server: &ServerInfo, name: &str, value: &str) -> SettingChange {
        if server.name() == "MySQL" && crate::version_at_least(&server.version, [8, 0, 0]) {
            return SettingChange {
                how: format!("Set {name} to {value}; SET PERSIST keeps it across restarts."),
                sql: Some(format!("SET PERSIST {name} = {};", literal(value))),
            };
        }
        SettingChange {
            how: format!(
                "Set {name} to {value} now, and in the server's option file (my.cnf) so it survives a restart."
            ),
            sql: Some(format!("SET GLOBAL {name} = {};", literal(value))),
        }
    }

    fn refresh_statistics(&self, target: &str) -> Fix {
        Fix {
            summary: "Recompute the table's statistics.".into(),
            sql: Some(self.dialect().refresh_statistics(target)),
            cost: None,
        }
    }

    fn wait_not_per_statement(&self, server: &ServerInfo, category: &str) -> String {
        match category {
            "lock" | "cpu" => "No statement recorded this in performance_schema.".into(),
            _ => format!("{} doesn't attribute this wait to statements; it is a server-wide total.", server.name()),
        }
    }

    fn index_advice_sources(&self, server: &ServerInfo, _has_workload: bool) -> Option<String> {
        Some(if server.has(Capability::PerformanceSchema) {
            format!(
                "{} doesn't record missing-index requests. Candidates come from captured plans, foreign keys, and rows \
                 read without an index (performance_schema).",
                server.name()
            )
        } else {
            format!(
                "{} doesn't record missing-index requests. Candidates come from the plans of logged statements and \
                 foreign keys; performance_schema would add rows read without an index.",
                server.name()
            )
        })
    }
}

impl Dialect for Mysql {
    fn quote(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn create_index(&self, index: &NewIndex) -> String {
        // InnoDB has no INCLUDE: secondary indexes already carry the primary key, so extra columns would have to become
        // keys. No partial indexes either.
        format!(
            "CREATE {}INDEX {}\n    ON {} ({})\n    ALGORITHM=INPLACE LOCK=NONE;",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        )
    }

    fn drop_index(&self, schema: &str, table: &str, index: &str) -> String {
        // MariaDB's DROP INDEX takes no ALGORITHM/LOCK options; ALTER TABLE takes them (comma-separated) on both.
        format!(
            "ALTER TABLE {} DROP INDEX {}, ALGORITHM=INPLACE, LOCK=NONE;",
            self.qualified(&[schema, table]),
            self.quote(index)
        )
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE TABLE {target};")
    }

    fn end_session(&self, session: &str) -> String {
        format!("KILL {session};")
    }
}
