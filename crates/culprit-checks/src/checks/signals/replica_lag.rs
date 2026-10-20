//! The `replica-lag` check: Replicas far behind the primary or stopped, and PostgreSQL replication slots that keep WAL for a consumer that's gone: pg_stat_replication, availability group replica states, SHOW REPLICA STATUS.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn replica_lag_info() -> CheckInfo {
    CheckInfo {
        id: "replica-lag".into(),
        title: "Replicas that fall behind".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Replicas far behind the primary or stopped, and PostgreSQL replication slots that keep WAL for a consumer that's gone: pg_stat_replication, availability group replica states, SHOW REPLICA STATUS.".into(),
    }
}

check!(
    ReplicaLagCheck: replica_lag_info, replica_lag,
    &[Evidence::Pressure { signal: None, timed_files: false, complete: false }]
);

/// How a replica, a consumer or this server stands, as replica-lag reads it.
#[derive(Debug, PartialEq)]
pub(super) enum ReplicaStatus {
    /// Replication failed or can't connect: the copy stopped with an error, or a standby meant to stream has no
    /// receiver.
    Stopped,
    /// Both MySQL replication threads are off and nothing failed: someone ran STOP REPLICA.
    StoppedByHand,
    /// The same on a server that takes writes: a former replica promoted to primary, its old configuration left behind.
    LeftConfigured,
    /// Behind by this many seconds beyond any configured delay (or by bytes, when that is all the engine says).
    Behind,
    Fine,
}

pub(super) fn replica_status(snapshot: &Snapshot, r: &Replica) -> ReplicaStatus {
    // Group Replication's own channels idle by design; the group reports its members elsewhere.
    if r.name.to_ascii_lowercase().starts_with("group_replication_") {
        return ReplicaStatus::Fine;
    }
    let state = r.state.as_deref().unwrap_or("");
    if state.starts_with("configured, not running") {
        return ReplicaStatus::LeftConfigured;
    }
    if state.starts_with("stopped by hand") {
        return ReplicaStatus::StoppedByHand;
    }
    if state.contains("stopped") || state == "not receiving" || state.eq_ignore_ascii_case("NOT SYNCHRONIZING") {
        return ReplicaStatus::Stopped;
    }
    let seconds = replica_lag_seconds(snapshot, r);
    // A configured delay keeps WAL waiting on purpose: only the seconds beyond it say whether it is behind.
    let bytes = r.lag_bytes.filter(|_| r.delay_seconds.is_none() && seconds.is_none_or(|s| s > 0.0));
    if seconds.is_some_and(|s| s >= 30.0) || bytes.is_some_and(|b| b >= GB) {
        ReplicaStatus::Behind
    } else {
        ReplicaStatus::Fine
    }
}

/// Seconds behind beyond any configured delay. A PostgreSQL standby with nothing received left to replay is caught up,
/// whatever the time since its last replayed commit (older scans reported that time as lag).
pub(super) fn replica_lag_seconds(snapshot: &Snapshot, r: &Replica) -> Option<f64> {
    r.effective_lag_seconds(snapshot.server.engine)
}

pub fn replica_lag(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    p.replicas
        .iter()
        .filter_map(|r| match r.role {
            ReplicaRole::Slot => slot_finding(snapshot, r),
            _ => replica_finding(snapshot, r),
        })
        .collect()
}

/// A replica, a logical consumer or this server, when it is stopped or behind.
pub(super) fn replica_finding(snapshot: &Snapshot, r: &Replica) -> Option<Finding> {
    // A logical consumer of another database on this server is that database's lag (its slot, kept server-wide, is
    // still reported as a slot).
    if r.is_logical() && r.database.as_deref().is_some_and(|d| !d.eq_ignore_ascii_case(&snapshot.server.database)) {
        return None;
    }
    let status = replica_status(snapshot, r);
    if status == ReplicaStatus::Fine {
        return None;
    }
    let pg = snapshot.server.engine == Engine::Postgres;
    let mysql = snapshot.server.engine == Engine::Mysql;
    let consumer = r.is_logical();
    let seconds = replica_lag_seconds(snapshot, r);
    let mut f = finding("replica-lag", &r.name, Category::Configuration, subject(SubjectKind::Database, &r.name));
    let lag = seconds
        .map(|s| format::duration(s * 1000.0))
        .or_else(|| r.lag_bytes.map(|b| format!("{} of log", format::bytes(b))))
        .unwrap_or_else(|| "behind".into());
    let state = r.state.clone().unwrap_or_default();
    match status {
        ReplicaStatus::LeftConfigured => {
            f.title = "This server is still set up as a replica, but replication isn't running".into();
            f.summary = "Both replication threads are off and no error is recorded, so someone stopped them, and this server takes writes (read_only is off). A former replica promoted to primary looks like this: its old replication settings stay behind, and starting them by mistake would apply another server's changes on top of this one's.".into();
            f.severity = Severity::Low;
            f.fix = Some(Fix {
                summary: "If it is no longer a replica, remove the configuration with RESET REPLICA ALL (RESET SLAVE ALL before MySQL 8.0.22 and MariaDB 10.5.1). If it should replicate, make it read-only and START REPLICA.".into(),
                sql: None,
                cost: None,
            });
        }
        ReplicaStatus::StoppedByHand => {
            f.title = "Replication to this server was stopped by hand".into();
            f.summary = "Both replication threads are off and no error is recorded: someone ran STOP REPLICA, for a backup or maintenance perhaps. While it stays stopped the copy gets older by the minute, and failing over to it would lose the changes since.".into();
            f.severity = Severity::Medium;
            f.fix = Some(Fix {
                summary: "If it should replicate, START REPLICA and let it catch up before relying on it; if it is no longer a replica, RESET REPLICA ALL removes the configuration.".into(),
                sql: None,
                cost: None,
            });
        }
        ReplicaStatus::Stopped => {
            f.title = match r.role {
                ReplicaRole::ThisServer => format!("Replication to this server has stopped ({state})"),
                _ if consumer => format!("The logical consumer {} isn't receiving changes ({state})", r.name),
                _ => format!("The replica {} isn't receiving changes ({state})", r.name),
            };
            let error = r.error.as_ref().map(|e| format!("The last error was {e}. ")).unwrap_or_default();
            f.summary = if pg && r.role == ReplicaRole::ThisServer && state == "not receiving" {
                format!(
                    "{error}This standby is set up to stream from its primary (primary_conninfo is set) but no WAL receiver is running, so it isn't getting changes: its data is getting older by the minute and failing over to it now would lose them."
                )
            } else {
                format!(
                    "{error}The copy no longer keeps up, so its data is getting older by the minute and failing over to it now would lose changes."
                )
            };
            f.severity = Severity::High;
            f.fix = Some(Fix {
                summary: if mysql {
                    "Read the full error with SHOW REPLICA STATUS on the replica, fix its cause, then START REPLICA."
                        .into()
                } else {
                    "Find why it stopped (the replica's error log, network, disk space), fix it, and let it catch up before relying on it.".into()
                },
                sql: None,
                cost: None,
            });
        }
        _ => {
            let delay = r.delay_seconds.map(|d| {
                format!(
                    " It is also set up to apply changes {} late on purpose ({}); that part isn't counted.",
                    format::duration(d * 1000.0),
                    if pg { "recovery_min_apply_delay" } else { "SOURCE_DELAY" }
                )
            });
            if consumer {
                f.title = format!("The logical consumer {} is {lag} behind", r.name);
                f.summary = format!(
                    "{} receives this database's changes through logical decoding: a subscription, or a CDC tool such as Debezium. It has confirmed changes only up to {lag} ago, so whatever it feeds sees data that old, and its slot keeps the WAL since then on this server. A consumer falls behind when it applies changes slower than this server writes them: one very large transaction, a slow destination, or a connector that is throttled or failing.",
                    r.name
                );
                f.fix = Some(Fix {
                    summary: "Look at the consumer, not this server: the subscriber's apply worker (pg_stat_subscription and its log) or the CDC connector's lag and errors. Split very large transactions so changes flow in smaller pieces.".into(),
                    sql: None,
                    cost: None,
                });
            } else {
                f.title = match r.role {
                    ReplicaRole::ThisServer => format!("This server is {lag} behind its primary"),
                    _ => format!("The replica {} is {lag} behind", r.name),
                };
                f.summary = format!(
                    "Reads from the replica return data {lag} old, and failing over now would lose or wait for that much. Lag grows when the replica applies changes slower than the primary writes them: large transactions, long queries on the replica{}, or slower storage.{}",
                    if pg { " (which can pause replay)" } else { "" },
                    delay.clone().unwrap_or_default()
                );
                f.fix = Some(Fix {
                    summary: "Split large write transactions into batches, check the replica's storage and CPU, and move long reports off it or let them run against a snapshot.".into(),
                    sql: None,
                    cost: None,
                });
            }
            f.severity = if seconds.is_some_and(|s| s >= 600.0) { Severity::High } else { Severity::Medium };
            if let Some(s) = seconds {
                f.metrics.push(Metric::new("Behind by", s * 1000.0, Unit::Ms));
            }
            if let Some(b) = r.lag_bytes {
                f.metrics.push(Metric::new("Not yet applied", b, Unit::Bytes));
            }
            if let Some(d) = r.delay_seconds {
                f.metrics.push(Metric::new("Delayed on purpose by", d * 1000.0, Unit::Ms));
            }
        }
    }
    severity_score(&mut f);
    f.impact.label = match status {
        ReplicaStatus::LeftConfigured | ReplicaStatus::StoppedByHand | ReplicaStatus::Stopped => {
            r.state.clone().unwrap_or_else(|| "stopped".into())
        }
        _ => format!("{lag} behind"),
    };
    f.confidence = Confidence::High;
    f.verify = vec![match status {
        ReplicaStatus::LeftConfigured => "Re-scan: the leftover replication configuration should be gone.".into(),
        _ => "Re-scan: the lag should be seconds.".into(),
    }];
    Some(f)
}

/// An inactive replication slot that keeps a gigabyte or more of WAL. A physical slot without its replica keeps WAL for
/// nothing; a logical one belongs to a consumer (a subscription, a CDC tool) that may only be between syncs, and
/// dropping it forces that consumer to copy everything again. Whether it can fill the disk depends on
/// max_slot_wal_keep_size (13+): with a cap, PostgreSQL gives the slot up instead.
pub(super) fn slot_finding(snapshot: &Snapshot, r: &Replica) -> Option<Finding> {
    let kept = r.lag_bytes.filter(|b| *b >= GB)?;
    if r.state.as_deref() != Some("inactive") || r.wal_status.as_deref() == Some("lost") {
        return None;
    }
    let logical = r.is_logical();
    // safe_wal_size exists from 13 and is empty when nothing caps what slots keep.
    let capped = r.safe_wal_bytes.is_some();
    let cap_known = r.wal_status.is_some();
    let mut f = finding("replica-lag", &r.name, Category::Configuration, subject(SubjectKind::Database, &r.name));
    f.title = if logical {
        format!("The logical slot {} keeps {} of WAL while its consumer is away", r.name, format::bytes(kept))
    } else {
        format!("The inactive replication slot {} keeps {} of WAL", r.name, format::bytes(kept))
    };
    let idle = r
        .inactive_seconds
        .map(|s| format!("{} has had no consumer for {}", r.name, format::duration(s * 1000.0)))
        .unwrap_or_else(|| format!("{} has no consumer connected", r.name));
    let kind = if logical {
        " It is a logical slot: a subscription or a CDC tool (Debezium, Fivetran, Airbyte…) reads changes through it, and many connect only between syncs. Dropping it makes that consumer copy everything again."
    } else {
        " It is a physical slot: a standby or a backup tool streams WAL through it."
    };
    // Before 13 nothing can cap a slot; from 13 the scan reads whether something does (older scans didn't).
    let can_cap = culprit_core::version_at_least(&snapshot.server.version, [13, 0, 0]);
    let risk = match (capped, cap_known, can_cap) {
        (true, _, _) => format!(
            " max_slot_wal_keep_size caps what it can keep: {} more WAL and PostgreSQL gives the slot up, so the disk is safe, but its consumer then has to start over.",
            format::bytes(r.safe_wal_bytes.unwrap_or(0.0))
        ),
        (false, true, _) => " Nothing caps what it keeps (max_slot_wal_keep_size is -1), so it grows until the disk fills and the server stops accepting writes.".into(),
        (false, false, true) => " Unless max_slot_wal_keep_size caps what slots keep, it grows until the disk fills and the server stops accepting writes.".into(),
        (false, false, false) => " Before PostgreSQL 13 nothing can cap it: it grows until the disk fills and the server stops accepting writes.".into(),
    };
    f.summary = format!(
        "A replication slot keeps every WAL file its consumer hasn't confirmed. {idle}, so the WAL it holds keeps growing.{kind}{risk}"
    );
    f.severity = match (capped, r.wal_status.as_deref()) {
        (true, Some("unreserved")) => Severity::Medium,
        (true, _) => Severity::Low,
        (false, _) if kept >= 10.0 * GB => Severity::High,
        _ => Severity::Medium,
    };
    let quoted = r.name.replace('\'', "''");
    // Only where nothing caps slots yet (or the scan couldn't tell): a cap already set is never lowered.
    let cap = (!capped && can_cap).then(|| setting_change(&snapshot.server, "max_slot_wal_keep_size", "50GB"));
    let mut sql = if logical {
        format!(
            "-- Only once its consumer is gone for good: dropping it makes the consumer copy everything again\n-- SELECT pg_drop_replication_slot('{}');",
            crate::support::comment_text(&quoted)
        )
    } else {
        format!("SELECT pg_drop_replication_slot('{quoted}');")
    };
    if let Some(statements) = cap.as_ref().and_then(|c| c.sql.as_deref()) {
        let lead = if cap_known {
            "Cap what any slot may keep"
        } else {
            "If max_slot_wal_keep_size isn't set, cap what any slot may keep"
        };
        sql.push_str(&format!("\n-- {lead}:\n{statements}"));
    }
    let cap_advice = match &cap {
        Some(c) if cap_known => format!(" To keep any slot from filling the disk, cap what slots may keep: {}", c.how),
        Some(c) => format!(
            " If max_slot_wal_keep_size isn't set, cap what slots may keep so none can fill the disk: {}",
            c.how
        ),
        None if !can_cap => " PostgreSQL 13 and later can cap what slots keep (max_slot_wal_keep_size).".into(),
        None => String::new(),
    };
    f.fix = Some(Fix {
        summary: if logical {
            format!(
                "Find its consumer first: the slot's name and plugin usually say which subscription or connector it is. If it still syncs, leave the slot: it catches up when the consumer connects. If the consumer is gone for good, drop it.{cap_advice}"
            )
        } else {
            format!("If nothing will stream through this slot again (a standby that was removed), drop it.{cap_advice}")
        },
        sql: Some(sql),
        cost: None,
    });
    f.metrics = vec![Metric::new("WAL kept", kept, Unit::Bytes)];
    if let Some(s) = r.inactive_seconds {
        f.metrics.push(Metric::new("Without a consumer for", s * 1000.0, Unit::Ms));
    }
    if let Some(b) = r.safe_wal_bytes {
        f.metrics.push(Metric::new("WAL left before it is given up", b, Unit::Bytes));
    }
    severity_score(&mut f);
    f.impact.label = format!("{} of WAL kept", format::bytes(kept));
    f.confidence = Confidence::High;
    f.verify = vec![if logical {
        "Re-scan once the consumer has synced: the WAL kept should drop, or the slot be gone.".into()
    } else {
        "Re-scan: the slot should be gone, or active again.".into()
    }];
    Some(f)
}
