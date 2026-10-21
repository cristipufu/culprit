use crate::{
    connect::Flavor,
    probe::Probe,
    rows::{query, query_with},
    sources, vars,
};
use culprit_core::operations::{ClusterHealth, OperationalCounter, OperationalCounterKind, SequenceCapacity};
use culprit_core::{Coverage, FileScope, Snapshot};
use mysql_async::{Conn, Value};

pub async fn collect(conn: &mut Conn, probe: &Probe, snapshot: &mut Snapshot) {
    let mut coverage = Coverage::default();
    if let Some(status) = coverage.read(&sources::REDO_PRESSURE, vars::status(conn, &["Innodb_log_waits"]).await)
        && let Some(count) = status.get("Innodb_log_waits")
    {
        snapshot.operations.counters.push(OperationalCounter {
            kind: OperationalCounterKind::RedoLogWaits,
            subject: "InnoDB redo buffer (whole server)".into(),
            scope: FileScope::Server,
            count: *count,
            since: probe.started_at.clone(),
            last_event_at: None,
            before: None,
        });
    }
    let sql = "SELECT CONCAT(t.TABLE_SCHEMA, '.', t.TABLE_NAME, '.', c.COLUMN_NAME) AS name,
        CONCAT(t.TABLE_SCHEMA, '.', t.TABLE_NAME) AS table_name, CAST(t.AUTO_INCREMENT AS CHAR) AS next_value,
        c.COLUMN_TYPE AS column_type, CAST(@@GLOBAL.auto_increment_increment AS CHAR) AS step
        FROM information_schema.TABLES t JOIN information_schema.COLUMNS c
        ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME
        WHERE t.TABLE_SCHEMA = ? AND c.EXTRA LIKE '%auto_increment%' AND t.AUTO_INCREMENT IS NOT NULL";
    if let Some(rows) = coverage
        .read_rows(&sources::SEQUENCE_CAPACITY, query_with(conn, sql, vec![Value::from(probe.database.clone())]).await)
    {
        for r in rows {
            let Some((minimum, maximum)) = integer_bounds(&r.string("column_type")) else {
                coverage.partial(
                    &sources::SEQUENCE_CAPACITY,
                    "An auto-increment column has an unsupported numeric type; its allocation range was not assessed.",
                );
                continue;
            };
            let (Some(current), Some(increment)) = (r.text("next_value"), r.text("step")) else { continue };
            snapshot.operations.sequences.push(SequenceCapacity {
                name: r.string("name"),
                table: r.text("table_name"),
                current,
                increment,
                minimum: minimum.into(),
                maximum: maximum.into(),
                cycles: false,
                allocated: true,
            });
        }
    }
    if probe.flavor == Flavor::Mariadb {
        let configuration = coverage.read(&sources::CLUSTER_HEALTH, vars::variables(conn, &["wsrep_on"]).await);
        let enabled = configuration.as_ref().is_some_and(|v| {
            v.iter().any(|(n, v)| n == "wsrep_on" && matches!(v.to_ascii_uppercase().as_str(), "ON" | "1"))
        });
        if configuration.is_some() && !enabled {
            // Replace the successful configuration probe: there is no Galera cluster to assess.
            coverage.not_applicable(&sources::CLUSTER_HEALTH, "Galera replication is not enabled on this server.");
        }
        if enabled && let Some(rows) = coverage.read(&sources::CLUSTER_HEALTH, query(conn,
            "SHOW GLOBAL STATUS WHERE Variable_name IN ('wsrep_ready','wsrep_connected','wsrep_cluster_status','wsrep_local_state_comment','wsrep_flow_control_paused')").await) {
            let read = |name: &str| rows.iter().find(|r| r.string("variable_name").eq_ignore_ascii_case(name)).and_then(|r| r.text("value"));
            let flag = |name: &str| read(name).map(|v| matches!(v.to_ascii_uppercase().as_str(),"ON"|"1"|"TRUE"));
            snapshot.operations.cluster = Some(ClusterHealth { ready: flag("wsrep_ready"), connected: flag("wsrep_connected"), primary_component: read("wsrep_cluster_status").map(|v| v.eq_ignore_ascii_case("Primary")), local_state: read("wsrep_local_state_comment"), flow_control_paused: read("wsrep_flow_control_paused").and_then(|v| v.parse().ok()) });
        }
    } else {
        coverage.not_applicable(
            &sources::CLUSTER_HEALTH,
            "Galera health is available only where MariaDB Galera is enabled.",
        );
    }
    coverage.append_to(snapshot);
}

fn integer_bounds(column_type: &str) -> Option<(&'static str, &'static str)> {
    let ty = column_type.to_ascii_lowercase();
    let unsigned = ty.contains("unsigned");
    let base = ty.split(['(', ' ']).next()?;
    Some(match (base, unsigned) {
        ("tinyint", false) => ("-128", "127"),
        ("tinyint", true) => ("0", "255"),
        ("smallint", false) => ("-32768", "32767"),
        ("smallint", true) => ("0", "65535"),
        ("mediumint", false) => ("-8388608", "8388607"),
        ("mediumint", true) => ("0", "16777215"),
        ("int" | "integer", false) => ("-2147483648", "2147483647"),
        ("int" | "integer", true) => ("0", "4294967295"),
        ("bigint", false) => ("-9223372036854775808", "9223372036854775807"),
        ("bigint", true) => ("0", "18446744073709551615"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_preserve_unsigned_bigint_precision() {
        assert_eq!(integer_bounds("bigint(20) unsigned"), Some(("0", "18446744073709551615")));
        assert_eq!(integer_bounds("decimal(20,0)"), None);
        assert_eq!(integer_bounds("int(11)"), Some(("-2147483648", "2147483647")));
    }
}
