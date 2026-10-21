//! Snapshot orchestration: collect independent sources concurrently and account for their coverage once.
use crate::{
    catalog::{columns, foreign_keys, indexes, settings, tables},
    connect::{COLLECTOR, Connector},
    probe::probe,
    rows::quote_ident,
    sources,
    workload::collect_workload,
};
use culprit_core::{
    Capability, ConnectionSpec, Coverage, Engine, Result, SNAPSHOT_FORMAT, Setting, Snapshot, SnapshotOptions,
    StatisticsInfo,
};
use std::time::Instant;

/// Collect a full snapshot. Each collector that fails becomes a note; only an unreachable server is an error.
pub async fn snapshot(
    connector: &Connector,
    spec: &ConnectionSpec,
    connection: &str,
    options: &SnapshotOptions,
) -> Result<Snapshot> {
    let started = Instant::now();
    let primary = connector.connect(COLLECTOR).await?;
    let probe = probe(&primary, &connector.host).await?;

    let (workload, schema, misc) = tokio::join!(
        collect_workload(&primary, &probe, options),
        async {
            let client = connector.connect(COLLECTOR).await?;
            Ok::<_, culprit_core::Error>((
                tables(&client, probe.version_num).await,
                indexes(&client, &probe).await,
                foreign_keys(&client).await,
                columns(&client).await,
            ))
        },
        async {
            let client = connector.connect(COLLECTOR).await?;
            Ok::<_, culprit_core::Error>((settings(&client).await, crate::pressure::collect(&client, &probe).await))
        },
    );

    let mut server = probe.server_info();
    // What the sources could read, starting from what the probe found (`culprit_core::Coverage`).
    let mut coverage = Coverage::with(std::mem::take(&mut server.capabilities), std::mem::take(&mut server.missing));

    let mut families = Vec::new();
    let mut statement_status = Vec::new();
    let (statements_ms, statements_io_ms) = match &workload {
        Ok(Some(c)) => (c.workload.total_time_ms, c.io_ms),
        _ => (0.0, 0.0),
    };
    let workload = match workload {
        Ok(Some(collected)) => {
            coverage.completed(&sources::STATEMENTS, collected.workload.queries.len());
            if let Some(message) = collected.plan_note {
                coverage.note("plans", message);
            }
            let withheld = collected.plans_withheld.is_some();
            if let Some(e) = collected.plans_withheld {
                coverage.lost(Capability::QueryPlans, e.message, e.hint);
            }
            families = collected.families;
            statement_status = collected.status;
            if !withheld && collected.workload.queries.iter().all(|q| q.plan.is_none()) {
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
        Err(e) => {
            coverage.source_failed(&sources::STATEMENTS, &e);
            None
        }
    };
    if !probe.privileged && probe.statements_usable() {
        coverage.partial(
            &sources::STATEMENTS,
            format!(
                "Statements run by other roles show as <insufficient privilege>. GRANT pg_read_all_stats TO {}; to see them.",
                quote_ident(&probe.user)
            ),
        );
    }

    // Each source through `Coverage::read`: one that fails is a note and its capabilities locked (`sources`).
    let (tables_out, indexes_out, fks_out, columns_out) = match schema {
        Ok((tables, indexes, fks, cols)) => (
            coverage.read_rows(&sources::TABLES, tables).unwrap_or_default(),
            coverage.read_rows(&sources::INDEXES, indexes).unwrap_or_default(),
            coverage.read_rows(&sources::FOREIGN_KEYS, fks).unwrap_or_default(),
            coverage.read_rows(&sources::COLUMNS, cols).unwrap_or_default(),
        ),
        // The connection they share failed: every one of them did.
        Err(e) => {
            for source in [&sources::TABLES, &sources::INDEXES, &sources::FOREIGN_KEYS, &sources::COLUMNS] {
                coverage.source_failed(source, &e);
            }
            Default::default()
        }
    };

    let (mut settings_out, pressure_out) = match misc {
        Ok((settings, (pressure, read))) => {
            coverage.merge(read);
            (coverage.read_rows(&sources::SETTINGS, settings).unwrap_or_default(), Some(pressure))
        }
        Err(e) => {
            coverage.sources_failed(sources::MISC, &e);
            (Vec::new(), None)
        }
    };
    settings_out.extend(statement_status);
    // A standby's usage counters are its own reads, next to the primary's statement history: say what it is, and the
    // engine sets its usage counters aside.
    if probe.standby {
        settings_out.push(Setting {
            name: culprit_core::REPLICA_SETTING.into(),
            value: "standby".into(),
            default_value: None,
            scope: "server".into(),
        });
    }
    // PostgreSQL keeps no wait history: time by category comes from statement and WAL timing, when they are on.
    let waits_out = crate::pressure::history_waits(&primary, &probe, statements_ms, statements_io_ms).await;
    if statements_io_ms > statements_ms {
        coverage.note("waits", "Recorded I/O time exceeds statement execution time; parallel or differently covered timing cannot be shown as exclusive shares of execution time. Per-query I/O totals remain available.");
    }

    if waits_out.is_empty() {
        coverage.withdraw(Capability::WaitStats);
    } else {
        coverage.delivered(Capability::WaitStats);
    }
    let sources = coverage.outcomes();
    let (have, missing, notes) = coverage.finish();
    server.capabilities = have;
    server.missing = missing;

    let statistics = tables_out
        .iter()
        .map(|t| StatisticsInfo {
            schema: t.schema.clone(),
            table: t.name.clone(),
            name: t.name.clone(),
            last_updated: t.last_analyze.clone(),
            rows: t.rows,
            rows_sampled: None,
            modifications: t.modifications_since_analyze,
            auto_created: false,
        })
        .collect();

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
        statistics,
        waits: waits_out,
        resources: Vec::new(),
        settings: settings_out,
        pressure: pressure_out,
        notes,
        options: Some(options.clone()),
        query_families: families,
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
        Engine::Postgres,
        &snapshot.server.database,
        &connector.host,
        connector.port,
        None,
    ));
    crate::signals::collect(&primary, &probe, &mut snapshot).await;
    snapshot.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(snapshot)
}
