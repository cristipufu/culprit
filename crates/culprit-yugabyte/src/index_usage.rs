use crate::{
    global,
    probe::{Probe, node_id},
    sources,
};
use culprit_core::{Capability, Coverage, Pressure, Result, Snapshot, SourceState};
use culprit_postgres::wire::{ObservedSession, query};
use std::collections::{BTreeMap, BTreeSet};

type Epochs = BTreeMap<String, Option<String>>;
async fn epochs(session: &ObservedSession, probe: &Probe) -> Result<Epochs> {
    let rows = query(
        &session.client,
        &format!(
            "SELECT server_uuid::text AS node,{} AS reset
        FROM pg_catalog.\"gv$pg_stat_database\" WHERE datid={}",
            crate::probe::timestamp("stats_reset"),
            probe.oid
        ),
    )
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                node_id(&r.string("node")).ok_or_else(|| culprit_core::Error::invalid("Unknown YSQL node identity"))?,
                r.text("reset"),
            ))
        })
        .collect()
}

pub(crate) async fn read(session: &ObservedSession, probe: &Probe, snapshot: &mut Snapshot, coverage: &mut Coverage) {
    if !probe.global {
        coverage.recorded(&sources::INDEX_USAGE, SourceState::Unavailable, Some("Node-local index counters cannot prove cluster-wide use or absence. Configured, enabled global views are required.".into()));
        coverage.lacking(Capability::IndexUsage, "Node-local index counters cannot establish cluster-wide usage",
            Some("Have an administrator configure and authorize YugabyteDB's global views before collecting cluster-wide index usage.".into()));
        return;
    }
    let notices = session.notices();
    let before = epochs(session, probe).await;
    let inventory = global::nodes(session).await;
    let rows = coverage.read_rows(
        &sources::INDEX_USAGE,
        query(
            &session.client,
            "SELECT schemaname,relname,indexrelname,sum(idx_scan)::float8 AS reads,
            count(DISTINCT server_uuid)::text AS nodes FROM pg_catalog.\"gv$pg_stat_user_indexes\"
         GROUP BY schemaname,relname,indexrelname",
        )
        .await,
    );
    let after = epochs(session, probe).await;
    let last_inventory = global::nodes(session).await;
    let complete = before
        .as_ref()
        .ok()
        .zip(after.as_ref().ok())
        .zip(inventory.as_ref().ok())
        .zip(last_inventory.as_ref().ok())
        .is_some_and(|(((a, b), expected), last)| {
            a == b && expected == last && &a.keys().cloned().collect::<BTreeSet<_>>() == expected
        })
        && session.notices() == notices;
    let Some(rows) = rows.filter(|_| complete) else {
        coverage.partial(&sources::INDEX_USAGE, "Global index contributors or database reset epochs changed, or a node was unavailable. No complete cluster usage is inferred from this read.");
        coverage.lacking(Capability::IndexUsage, "Complete cluster index contributors could not be established", None);
        return;
    };
    let epochs = before.unwrap_or_default();
    let reset = epochs
        .values()
        .map(|at| at.as_deref().filter(|at| culprit_core::format::epoch_seconds(at).is_some()))
        .collect::<Option<Vec<_>>>()
        .and_then(|dates| dates.into_iter().max());
    let count = epochs.len();
    let mut usage = BTreeMap::new();
    let mut partial = false;
    for row in rows {
        if row.string("nodes").parse::<usize>().ok() != Some(count) {
            partial = true;
            continue;
        }
        usage.insert(
            (row.string("schemaname"), row.string("relname"), row.string("indexrelname")),
            row.number("reads").filter(|n| *n >= 0.0),
        );
    }
    for index in &mut snapshot.indexes {
        let key = (index.schema.clone(), index.table.clone(), index.name.clone());
        if let Some(reads) = usage.get(&key) {
            index.reads = *reads;
            index.counters_since = reset.map(str::to_string);
        } else {
            partial = true;
        }
    }
    if partial {
        coverage.partial(
            &sources::INDEX_USAGE,
            "Some indexes did not supply usage from every current node; those counts remain unknown.",
        );
        coverage.lacking(Capability::IndexUsage, "Some indexes lack counters from every current node", None);
    } else {
        coverage.delivered(Capability::IndexUsage);
    }
    let common_reset = reset.filter(|at| epochs.values().all(|value| value.as_deref() == Some(*at)));
    if let Some(hours) = common_reset
        .and_then(culprit_core::format::epoch_seconds)
        .zip(culprit_core::format::epoch_seconds(&probe.now))
        .map(|(a, b)| (b - a) / 3600.0)
        .filter(|h| *h >= 0.0)
    {
        snapshot.pressure.get_or_insert_with(Pressure::default).uptime_hours = Some(hours);
    } else {
        coverage.note("YSQL index history", "Cluster read counts do not share a common reset epoch. Rates are unavailable; absence of reads is assessed only when every node supplies a known reset date covering at least a week.");
    }
}
