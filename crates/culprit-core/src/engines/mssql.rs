//! SQL Server and Azure SQL.

use super::*;
use crate::dialect::{NewIndex, Online, included, keys, unique};
use crate::model::Capability;

pub(super) struct Mssql;

impl EngineSpec for Mssql {
    fn statement_metrics(&self) -> StatementMetrics {
        StatementMetrics {
            planning_label: "Compiling".into(),
            compilation_history: true,
            lock_time: true,
            write_rows_label: Some("Rows per run".into()),
            errors_note: Some(
                "Query Store counts errors but doesn't keep them; the finding shows how to capture them.".into(),
            ),
            ..StatementMetrics::BASIC
        }
    }
    fn wait_sampling(&self) -> Option<WaitSampling> {
        Some(WaitSampling::Counters)
    }
    fn sql_syntax(&self) -> SqlSyntax {
        SqlSyntax::Mssql
    }
    fn inactive_index_state(&self, _server: &ServerInfo) -> crate::IndexState {
        crate::IndexState::Disabled
    }
    fn limitations(&self) -> Vec<String> {
        vec![
        "Query Store records aggregate executions and plan history, not an application's request trace or every parameter value. Caller identity and transaction boundaries need application telemetry.".into(),
        "An estimated or stored compilation plan does not measure the rows, timing or waits of the run you are investigating. Runtime-only problems require recorded runtime evidence or an explicitly measured run.".into(),
        "Host CPU and filesystem capacity need their own measurements, such as connected cloud telemetry. Backup recoverability and database corruption are not checked by the statistics scan; they need backup/recovery validation and an appropriate integrity investigation.".into(),
    ]
    }
    fn label(&self) -> &'static str {
        "SQL Server"
    }

    fn connect(&self) -> Connect {
        Connect::server(
            "SQL Server",
            "Server=tcp:shop.database.windows.net;Database=shop;User ID=app",
            "An ADO.NET connection string, an mssql:// or JDBC URL, or host,port/database. Azure SQL without a user name signs in with your az login (Entra ID).",
        )
    }

    fn abilities(&self) -> EngineAbilities {
        EngineAbilities { last_actual_plan: true, operator_progress: true, ..SERVER }
    }

    fn session_actions(&self) -> Vec<SessionAction> {
        vec![SessionAction::Terminate]
    }

    fn waiting_state(&self) -> &'static str {
        "suspended"
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &Mssql
    }

    fn change_setting(&self, _server: &ServerInfo, name: &str, value: &str) -> SettingChange {
        SettingChange {
            how: format!("Set {name} to {value}."),
            sql: Some(format!("EXEC sp_configure '{name}', {};\nRECONFIGURE;", literal(value))),
        }
    }

    fn refresh_statistics(&self, target: &str) -> Fix {
        Fix {
            summary: "Update the statistics now; if this table changes fast, schedule the update in a maintenance job."
                .into(),
            sql: Some(self.dialect().refresh_statistics(target)),
            cost: Some("Reads a sample of the table; run it outside peak hours on very large tables.".into()),
        }
    }

    fn wait_not_per_statement(&self, _server: &ServerInfo, _category: &str) -> String {
        "Query Store didn't record this wait for individual queries in the window.".into()
    }

    fn index_advice_sources(&self, server: &ServerInfo, _has_workload: bool) -> Option<String> {
        (!server.has(Capability::MissingIndexes)).then(|| {
            "This login can't read SQL Server's missing-index requests (VIEW DATABASE STATE), so candidates come from \
             captured plans and foreign keys only."
                .to_string()
        })
    }
}

impl Dialect for Mssql {
    fn quote(&self, ident: &str) -> String {
        format!("[{}]", ident.replace(']', "]]"))
    }

    fn create_index(&self, index: &NewIndex) -> String {
        let mut sql = format!(
            "CREATE {}NONCLUSTERED INDEX {}\n    ON {} ({})",
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
        match index.online {
            Online::Yes => sql.push_str("\n    WITH (ONLINE = ON);"),
            Online::No => sql.push(';'),
            Online::Unknown => sql.push_str(
                ";\n-- Add WITH (ONLINE = ON) on Enterprise or Azure SQL to keep the table available while it builds.",
            ),
        }
        sql
    }

    fn drop_index(&self, schema: &str, table: &str, index: &str) -> String {
        format!("DROP INDEX {} ON {};", self.quote(index), self.qualified(&[schema, table]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("UPDATE STATISTICS {target};")
    }

    fn end_session(&self, session: &str) -> String {
        format!("KILL {session};")
    }
}
