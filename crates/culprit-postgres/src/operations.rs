//! Operational failures and allocation limits from the current database's catalogs and statistics.
use crate::{
    rows::{iso, query},
    sources,
};
use culprit_core::operations::{FreezeAge, OperationalCounter, OperationalCounterKind as Kind, SequenceCapacity};
use culprit_core::{Coverage, FileScope, Snapshot};
use tokio_postgres::Client;

pub async fn collect(client: &Client, version: u32, snapshot: &mut Snapshot) {
    let mut coverage = Coverage::default();
    let age_sql = "SELECT current_database() AS subject, mxid_age(datminmxid)::float8 AS age,
 current_setting('autovacuum_multixact_freeze_max_age')::float8 AS freeze_max
 FROM pg_database WHERE datname = current_database()
 UNION ALL SELECT n.nspname || '.' || c.relname, mxid_age(c.relminmxid)::float8,
 COALESCE((SELECT split_part(o, '=', 2)::float8 FROM unnest(c.reloptions) o WHERE o LIKE 'autovacuum_multixact_freeze_max_age=%'), current_setting('autovacuum_multixact_freeze_max_age')::float8)
 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'm') AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_temp_%'";
    if let Some(rows) = coverage.read(&sources::MULTIXACTS, query(client, age_sql).await) {
        snapshot.operations.multixacts = rows
            .iter()
            .filter_map(|r| {
                Some(FreezeAge {
                    subject: r.string("subject"),
                    age: r.number("age")?,
                    freeze_max_age: r.number("freeze_max"),
                })
            })
            .collect();
    }
    let archive = format!(
        "SELECT failed_count::float8 AS count, {} AS since, {} AS last_at FROM pg_stat_archiver",
        iso("stats_reset"),
        iso("last_failed_time")
    );
    if let Some(rows) = coverage.read(&sources::ARCHIVER, query(client, &archive).await) {
        for r in rows {
            if let Some(count) = r.number("count") {
                snapshot.operations.counters.push(OperationalCounter {
                    kind: Kind::ArchiveFailures,
                    subject: "WAL archive (whole server)".into(),
                    scope: FileScope::Server,
                    count,
                    since: r.text("since"),
                    last_event_at: r.text("last_at"),
                    before: None,
                });
            }
        }
    }
    if version >= 150_000 {
        let sql = format!("SELECT sub.subname, st.apply_error_count::float8 AS apply_errors, st.sync_error_count::float8 AS sync_errors, {} AS since
 FROM pg_stat_subscription_stats st JOIN pg_subscription sub ON sub.oid = st.subid
 WHERE sub.subdbid = (SELECT oid FROM pg_database WHERE datname = current_database())", iso("st.stats_reset"));
        if let Some(rows) = coverage.read(&sources::SUBSCRIPTION_ERRORS, query(client, &sql).await) {
            for r in rows {
                for (kind, column) in
                    [(Kind::SubscriptionApplyErrors, "apply_errors"), (Kind::SubscriptionSyncErrors, "sync_errors")]
                {
                    if let Some(count) = r.number(column) {
                        snapshot.operations.counters.push(OperationalCounter {
                            kind,
                            subject: r.string("subname"),
                            scope: FileScope::Database,
                            count,
                            since: r.text("since"),
                            last_event_at: None,
                            before: None,
                        });
                    }
                }
            }
        }
    }
    let conflicts = format!("SELECT (c.confl_tablespace + c.confl_lock + c.confl_snapshot + c.confl_bufferpin + c.confl_deadlock)::float8 AS count, {} AS since
 FROM pg_stat_database_conflicts c JOIN pg_stat_database d ON d.datid = c.datid
 WHERE c.datname = current_database() AND pg_is_in_recovery()", iso("d.stats_reset"));
    if let Some(rows) = coverage.read(&sources::STANDBY_CONFLICTS, query(client, &conflicts).await) {
        for r in rows {
            if let Some(count) = r.number("count") {
                snapshot.operations.counters.push(OperationalCounter {
                    kind: Kind::StandbyConflicts,
                    subject: snapshot.server.database.clone(),
                    scope: FileScope::Database,
                    count,
                    since: r.text("since"),
                    last_event_at: None,
                    before: None,
                });
            }
        }
    }
    if version >= 100_000 {
        let seq = "SELECT schemaname || '.' || sequencename AS name, last_value::text AS current_value,
 increment_by::text AS step, min_value::text AS min_value, max_value::text AS max_value, cycle
 FROM pg_sequences WHERE last_value IS NOT NULL AND schemaname NOT IN ('pg_catalog', 'information_schema')";
        if let Some(rows) = coverage.read(&sources::SEQUENCE_CAPACITY, query(client, seq).await) {
            for r in rows {
                let (Some(current), Some(increment), Some(minimum), Some(maximum)) =
                    (r.text("current_value"), r.text("step"), r.text("min_value"), r.text("max_value"))
                else {
                    continue;
                };
                snapshot.operations.sequences.push(SequenceCapacity {
                    name: r.string("name"),
                    table: None,
                    current,
                    increment,
                    minimum,
                    maximum,
                    cycles: r.flag("cycle"),
                    allocated: true,
                });
            }
        }
    }
    snapshot.notes.extend(coverage.finish().2);
}
