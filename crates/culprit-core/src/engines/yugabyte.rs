//! YugabyteDB: PostgreSQL's SQL grammar over DocDB's distributed LSM storage.
use super::postgres::Postgres;
use super::*;
use crate::dialect::{NewIndex, included, unique};

pub(super) struct Yugabyte;

impl EngineSpec for Yugabyte {
    fn sql_syntax(&self) -> SqlSyntax {
        SqlSyntax::Postgres
    }
    fn label(&self) -> &'static str {
        "YugabyteDB"
    }
    fn inactive_index_state(&self, _: &ServerInfo) -> crate::IndexState {
        crate::IndexState::Invalid
    }
    fn abilities(&self) -> EngineAbilities {
        EngineAbilities { deadlocks: false, ..SERVER }
    }
    fn session_actions(&self) -> Vec<SessionAction> {
        vec![SessionAction::Cancel, SessionAction::Terminate]
    }
    fn connect(&self) -> Connect {
        Connect::server(
            "YugabyteDB",
            "yugabyte://yugabyte@db.example.com:5433/yugabyte",
            "A YSQL postgres:// or yugabyte:// URL, libpq parameters, or host:port/database.",
        )
    }
    fn dialect(&self) -> &'static dyn Dialect {
        &Yugabyte
    }
    fn limitations(&self) -> Vec<String> {
        vec![
            "YSQL uses DocDB/RocksDB over Raft. PostgreSQL heap pages, buffer-cache ratios, dead tuples, vacuum and XID ages are not storage measurements here.".into(),
            "Local statistics cover the connected YB-TServer. Configured cluster views add other nodes; skipped nodes and independent counter resets prevent complete comparisons.".into(),
            "Captured generic plans use this login's current role and settings. Actual DocDB RPCs and rows scanned require EXPLAIN ANALYZE with DIST.".into(),
            "An index's sharding keys affect distributed reads and write hotspots. Suggested ordered keys use explicit ASC/DESC; review partitioning before deployment.".into(),
        ]
    }
    fn deadlock_limitation(&self) -> Option<&'static str> {
        Some(
            "DocDB's distributed deadlock detection is not PostgreSQL's heap lock accounting. A PostgreSQL deadlock counter does not establish this database's distributed deadlocks.",
        )
    }
    fn refusal(&self, ability: Ability) -> Refusal {
        match ability {
            Ability::Deadlocks => Refusal::new(
                self.deadlock_limitation().unwrap(),
                Some("Correlate YSQL errors and the YB-TServer logs for the affected transaction."),
            ),
            Ability::LastActualPlan => Refusal::new(
                "YSQL statement totals do not retain the measured plan of every past execution",
                Some("Analyze a representative statement to capture its actual DocDB work."),
            ),
            Ability::OperatorProgress => Refusal::new(
                "YSQL does not expose live row counters for every plan operator",
                Some("Use a measured plan and native statement/operation progress."),
            ),
            _ => Refusal::new(&format!("YugabyteDB cannot provide {}", wire_name(&ability)), None),
        }
    }
    fn mutating_function(&self, name: &str) -> bool {
        name.starts_with("yb_")
            && !matches!(
                name,
                "yb_servers"
                    | "yb_servers_metrics"
                    | "yb_lock_status"
                    | "yb_table_properties"
                    | "yb_is_local_table"
                    | "yb_get_local_tserver_uuid"
                    | "yb_server_region"
                    | "yb_server_cloud"
                    | "yb_server_zone"
                    | "yb_hash_code"
                    | "yb_is_database_colocated"
            )
    }
    fn database_setting(&self, server: &ServerInfo, name: &str, value: &str) -> Option<SettingChange> {
        let name = [
            "work_mem",
            "statement_timeout",
            "lock_timeout",
            "idle_in_transaction_session_timeout",
            "default_statistics_target",
        ]
        .into_iter()
        .find(|n| n.eq_ignore_ascii_case(name))?;
        Some(SettingChange {
            how: format!("Set {name} for new YSQL connections to this database."),
            sql: Some(format!(
                "ALTER DATABASE {} SET {name} = '{}';",
                self.quote(&server.database),
                value.replace('\'', "''")
            )),
        })
    }
    fn change_setting(&self, server: &ServerInfo, name: &str, value: &str) -> SettingChange {
        self.database_setting(server, name, value).unwrap_or_else(|| SettingChange {
            how: format!("Configure the supported YSQL or DocDB parameter {name} through YugabyteDB's administration tools; review its scope and restart requirements."), sql: None,
        })
    }
    fn refresh_statistics(&self, target: &str) -> Fix {
        Fix {
            summary: "Refresh YSQL's optimizer statistics. This does not vacuum or compact DocDB storage.".into(),
            sql: Some(self.dialect().refresh_statistics(target)),
            cost: Some("ANALYZE samples table data and can issue distributed reads.".into()),
        }
    }
    fn wait_not_per_statement(&self, _: &ServerInfo, _: &str) -> String {
        "DocDB RPC and storage wait timings may overlap; they are not exclusive shares of statement execution time. Use their native metrics or sample active sessions.".into()
    }
    fn index_advice_sources(&self, _: &ServerInfo, has_workload: bool) -> Option<String> {
        Some(if has_workload { "Candidates come from YSQL plans, the index catalog and foreign keys; DocDB does not publish missing-index requests." }
             else { "Without statement statistics, candidates come from foreign keys and the index catalog." }.into())
    }
}

impl Dialect for Yugabyte {
    fn quote(&self, name: &str) -> String {
        Postgres.quote(name)
    }
    fn create_index(&self, index: &NewIndex) -> String {
        // A bare first key defaults to HASH outside enhanced compatibility mode, and cannot serve a range filter.
        let keys = index
            .keys
            .iter()
            .map(|key| {
                let name = crate::model::strip_direction(key);
                let descending = key.to_ascii_uppercase().ends_with(" DESC");
                format!("{} {}", self.quote(name), if descending { "DESC" } else { "ASC" })
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql =
            format!("CREATE {}INDEX {}\n    ON {} ({keys})", unique(index), self.quote(index.name), index.target);
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
    fn drop_index(&self, schema: &str, _: &str, name: &str) -> String {
        format!("DROP INDEX {};", self.qualified(&[schema, name]))
    }
    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE {target};")
    }
    fn end_session(&self, session: &str) -> String {
        let parts: Vec<_> = session.split('/').collect();
        let [node, pid, started] = parts.as_slice() else {
            return "-- Select a node-qualified YSQL session from Live.".into();
        };
        let node = node.replace('-', "");
        let Some(pid) = pid.parse::<i32>().ok().filter(|p| *p > 0) else { return "-- Invalid YSQL backend id.".into() };
        if node.len() != 32
            || !node.bytes().all(|b| b.is_ascii_hexdigit())
            || crate::format::epoch_seconds(started).is_none()
        {
            return "-- Invalid YSQL session identity.".into();
        }
        format!(
            "SELECT pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a\nWHERE a.pid = {pid} AND a.datname = pg_catalog.current_database()\n  AND a.backend_type = 'client backend' AND a.pid <> pg_catalog.pg_backend_pid()\n  AND COALESCE(a.application_name, '') <> 'culprit' AND a.backend_start = '{}'::timestamptz\n  AND pg_catalog.replace(pg_catalog.yb_get_local_tserver_uuid()::text, '-', '') = '{}';",
            started.replace('\'', "''"),
            node.to_ascii_lowercase()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexes_preserve_range_access_and_session_actions_require_node_identity() {
        let keys = vec!["created_at".into(), "id DESC".into()];
        let sql = Yugabyte.create_index(&NewIndex {
            target: "public.orders",
            unique: false,
            name: "by_time",
            keys: &keys,
            include: &[],
            filter: None,
            online: crate::Online::Unknown,
        });
        assert!(sql.contains("(created_at ASC, id DESC)"));
        assert!(!sql.contains("CONCURRENTLY"));
        assert!(Yugabyte.end_session("42").starts_with("--"));
        let sql = Yugabyte.end_session("3d2d9e10-555b-4ae2-a683-a8402505e416/42/2026-10-23T21:00:00Z");
        assert!(
            sql.contains("yb_get_local_tserver_uuid")
                && sql.contains("backend_start")
                && sql.contains("client backend")
        );
    }
}
