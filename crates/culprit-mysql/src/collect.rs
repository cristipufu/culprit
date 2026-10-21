//! Snapshot orchestration: collect independent source groups and account for their coverage once.
use crate::{
    catalog::{columns, foreign_keys, indexes, replica_role, settings, tables},
    connect::{Connector, Scope},
    probe::probe,
    slowlog, sources,
    workload::collect_workload,
};
use culprit_core::{
    Capability, ConnectionSpec, Coverage, Engine, Error, Result, SNAPSHOT_FORMAT, Setting, Snapshot, SnapshotOptions,
};
use std::time::Instant;

const PS_GRANT: &str = "GRANT SELECT ON performance_schema.* TO";

/// Collect a full snapshot. Each collector that fails becomes a note; only an unreachable server is an error.
pub async fn snapshot(
    connector: &Connector,
    spec: &ConnectionSpec,
    connection: &str,
    options: &SnapshotOptions,
) -> Result<Snapshot> {
    let started = Instant::now();
    let mut primary = connector.connect(Scope::Server).await?;
    let probe = probe(&mut primary, connector).await?;
    let db = probe.database.clone();

    let (workload, schema, misc) = tokio::join!(
        async {
            let mut conn = primary;
            collect_workload(&mut conn, connector, &probe, options).await
        },
        async {
            let mut conn = connector.connect(Scope::Server).await?;
            let tables = tables(&mut conn, &db).await;
            let columns = columns(&mut conn, &db).await;
            let indexes = indexes(&mut conn, &db, &probe).await;
            let fks = foreign_keys(&mut conn, &db).await;
            Ok::<_, Error>((tables, columns, indexes, fks))
        },
        async {
            let mut conn = connector.connect(Scope::Server).await?;
            let mut settings = settings(&mut conn, probe.flavor).await;
            if let (Ok(settings), Some(role)) = (settings.as_mut(), replica_role(&mut conn, &probe).await) {
                settings.push(Setting {
                    name: culprit_core::REPLICA_SETTING.into(),
                    value: role.into(),
                    default_value: None,
                    scope: "server".into(),
                });
            }
            let waits = crate::pressure::waits(&mut conn).await;
            let pressure = crate::pressure::collect(&mut conn).await;
            Ok::<_, Error>((settings, waits, pressure))
        },
    );

    let mut server = probe.server_info();
    // What the sources could read, starting from what the probe found (`culprit_core::Coverage`).
    let mut coverage = Coverage::with(std::mem::take(&mut server.capabilities), std::mem::take(&mut server.missing));

    let mut source_waits = Vec::new();
    let workload = match workload {
        Ok(Some(collected)) => {
            coverage.completed(&sources::STATEMENTS, collected.workload.queries.len());
            source_waits = collected.waits;
            for (collector, message) in collected.notes {
                coverage.note(collector, message);
            }
            if collected.workload.source == slowlog::SOURCE {
                coverage.delivered(Capability::SlowLog);
            }
            let planned = collected.workload.queries.iter().any(|q| q.plan.is_some());
            if planned {
                coverage.delivered(Capability::QueryPlans);
            } else if let Some((reason, fix)) = collected.no_plans {
                coverage.lost(Capability::QueryPlans, reason, fix);
            } else {
                coverage.withdraw(Capability::QueryPlans);
            }
            Some(collected.workload)
        }
        Ok(None) => {
            coverage.recorded(
                &sources::STATEMENTS,
                culprit_core::SourceState::Unavailable,
                Some("Statement statistics are not enabled or available on this database.".into()),
            );
            None
        }
        // The digests' grant is the fix whatever the error said.
        Err(e) => {
            coverage.source_failed(&sources::STATEMENTS, &e.with_hint(format!("{PS_GRANT} {};", probe.grant_target())));
            None
        }
    };

    // Each source through `Coverage::read`: one that fails is a note and its capabilities locked (`sources`).
    let (tables_out, columns_out, indexes_out, fks_out) = match schema {
        Ok((tables, columns, indexes, fks)) => {
            let indexes = coverage.read(&sources::INDEXES, indexes).map(|(i, usage_note)| {
                coverage.completed(&sources::INDEXES, i.len());
                // Read, but without usage counters: the indexes stand, the counters are locked with why.
                if let Some(message) = usage_note {
                    coverage.note("index_usage", message.clone());
                    coverage.lost(
                        Capability::IndexUsage,
                        message,
                        Some(format!("{PS_GRANT} {};", probe.grant_target())),
                    );
                }
                i
            });
            (
                coverage.read_rows(&sources::TABLES, tables).unwrap_or_default(),
                coverage.read_rows(&sources::COLUMNS, columns).unwrap_or_default(),
                indexes.unwrap_or_default(),
                coverage.read_rows(&sources::FOREIGN_KEYS, fks).unwrap_or_default(),
            )
        }
        // The connection they share failed: every one of them did.
        Err(e) => {
            for source in [&sources::TABLES, &sources::COLUMNS, &sources::INDEXES, &sources::FOREIGN_KEYS] {
                coverage.source_failed(source, &e);
            }
            Default::default()
        }
    };

    let (settings_out, mut waits_out, pressure_out) = match misc {
        Ok((settings, waits, (pressure, read))) => {
            coverage.merge(read);
            let waits = coverage.read_rows(&sources::WAITS, waits).unwrap_or_default();
            if !waits.is_empty() {
                coverage.delivered(Capability::WaitStats);
            }
            (coverage.read_rows(&sources::SETTINGS, settings).unwrap_or_default(), waits, Some(pressure))
        }
        Err(e) => {
            coverage.sources_failed(sources::MISC, &e);
            (Vec::new(), Vec::new(), None)
        }
    };
    if waits_out.is_empty() {
        waits_out = std::mem::take(&mut source_waits);
    }

    let sources = coverage.outcomes();
    let (have, missing, notes) = coverage.finish();
    server.capabilities = have;
    server.missing = missing;

    let mut snapshot = Snapshot {
        identity: None,
        sources,
        format: SNAPSHOT_FORMAT,
        id: probe.now.replace(['-', ':'], ""),
        connection: connection.to_string(),
        environment: spec.environment,
        taken_at: probe.now.clone(),
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        server,
        workload,
        tables: tables_out,
        indexes: indexes_out,
        columns: columns_out,
        foreign_keys: fks_out,
        missing_indexes: Vec::new(),
        statistics: Vec::new(),
        waits: waits_out,
        resources: Vec::new(),
        settings: settings_out,
        pressure: pressure_out,
        notes,
        options: Some(options.clone()),
        query_families: Vec::new(),
        plan_cache: None,
        tuning: None,
        deadlocks: None,
        constraints: Vec::new(),
        histograms: None,
        escalations_before: None,
        contention_before: None,
        evictions_before: None,
        sizes_before: None,
        operations: Default::default(),
    };
    snapshot.bind_identity(culprit_core::DatabaseIdentity::server(
        Engine::Mysql,
        &snapshot.server.database,
        &connector.host,
        connector.port,
        None,
    ));
    crate::signals::collect(connector, &probe, &mut snapshot).await;
    snapshot.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(snapshot)
}
