//! The `held-cleanup` check: The transaction that holds this database's cleanup back: one with uncommitted changes or row versions here (SQL Server), the oldest snapshot, replication slot or prepared transaction behind VACUUM's horizon (PostgreSQL), or the oldest read view behind InnoDB's purge (MySQL).

use super::*;

pub fn held_cleanup_info() -> CheckInfo {
    CheckInfo {
        id: "held-cleanup".into(),
        title: "A long open transaction holds cleanup back".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "The transaction that holds this database's cleanup back: one with uncommitted changes or row versions here (SQL Server), the oldest snapshot, replication slot or prepared transaction behind VACUUM's horizon (PostgreSQL), or the oldest read view behind InnoDB's purge (MySQL).".into(),
    }
}

check!(HeldCleanupCheck: held_cleanup_info, held_cleanup);

/// Transactions behind at which a replication slot's hold on VACUUM is worth a finding, and High.
pub(super) const SLOT_BEHIND: f64 = 1_000_000.0;
const SLOT_FAR_BEHIND: f64 = 100_000_000.0;

pub fn held_cleanup(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(t) = pressure(snapshot).and_then(|p| p.oldest_transaction.as_ref()) else { return Vec::new() };
    let engine = snapshot.server.engine;
    // Collectors report only what holds this database's cleanup; scans before they said what it is held a session.
    let holder = t.holder.unwrap_or(CleanupHolder::Session);
    let slot = holder == CleanupHolder::ReplicationSlot;
    let held_gb = t.held_bytes.unwrap_or(0.0) / 1024.0 / 1024.0 / 1024.0;
    let behind = t.xid_age.unwrap_or(0.0);
    // How long it has been open is what tells a forgotten transaction from a busy one; a slot has no start time, so
    // it is how far behind it holds the horizon.
    let old = if slot { behind >= SLOT_BEHIND } else { t.seconds >= 1800.0 };
    let big = engine == Engine::Mssql && held_gb >= 1.0 && t.seconds >= 300.0;
    if !old && !big {
        return Vec::new();
    }
    let open = format::duration(t.seconds * 1000.0);
    let idle = t.state.as_deref().is_some_and(|s| s.contains("idle"));
    let snapshot_reader = t.held_bytes.is_some_and(|b| b > 0.0) || t.state.as_deref() == Some("snapshot");
    let wrote = t.wrote == Some(true);
    let other_database = t.database.as_deref().filter(|d| !d.eq_ignore_ascii_case(&snapshot.server.database));
    let mut f = finding(
        "held-cleanup",
        &t.session,
        Category::Maintenance,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    f.title = match (engine, holder) {
        (Engine::Mssql, _) => match t.held_bytes {
            Some(b) if b > 0.0 => format!(
                "Session {} has kept a transaction open for {open}; this database's version store holds {}",
                t.session,
                format::bytes(b)
            ),
            _ if wrote => {
                format!("Session {} has kept a transaction with uncommitted changes open for {open}", t.session)
            }
            _ => format!("Session {} has kept a transaction open for {open}", t.session),
        },
        (Engine::Postgres, CleanupHolder::ReplicationSlot) => {
            format!(
                "Replication slot {} holds {} back by {} transactions",
                t.session,
                if t.catalog_only { "system-catalog cleanup" } else { "VACUUM" },
                format::count(behind)
            )
        }
        (Engine::Postgres, CleanupHolder::PreparedTransaction) => format!(
            "Prepared transaction '{}' has waited {open} to be committed or rolled back, so VACUUM can't clean up behind it",
            t.session
        ),
        (Engine::Postgres, CleanupHolder::Session) => format!(
            "Session {} has kept a transaction open for {open}{}, so VACUUM can't clean up behind it",
            t.session,
            if idle { " doing nothing" } else { "" }
        ),
        (Engine::Mysql, _) => {
            format!("A transaction in session {} has been open for {open}; check its locks and read view", t.session)
        }
        (Engine::Sqlite, _) => format!("A transaction has been open for {open}"),
    };
    let program =
        t.program.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" It comes from {p}.")).unwrap_or_default();
    let behind_note = if behind > 0.0 {
        format!(" {} transactions have started since its snapshot.", format::count(behind))
    } else {
        String::new()
    };
    f.summary = match (engine, holder) {
        (Engine::Mssql, _) => {
            let mut parts = Vec::new();
            if snapshot_reader {
                parts.push("It reads or makes row versions, and while it stays open SQL Server keeps every version this database made since it started in the version store; reads have to walk past them.".to_string());
            }
            if wrote {
                parts.push("It changed data in this database and hasn't committed: SQL Server can't reuse the transaction log past its first change, so the log grows, and its locks stay held.".to_string());
            }
            if parts.is_empty() {
                parts.push(
                    "An open transaction keeps its locks, and the log can't be reused past its first change."
                        .to_string(),
                );
            }
            format!("{}{program} Short transactions keep both small.", parts.join(" "))
        }
        (Engine::Postgres, CleanupHolder::ReplicationSlot) if t.catalog_only => format!(
            "This replication slot's catalog_xmin keeps old system-catalog row versions for logical decoding, {} transactions behind. It does not establish that this slot holds back cleanup of application-table rows. Check the consumer's progress and recovery requirements before changing or removing a slot.",
            format::count(behind)
        ),
        (Engine::Postgres, CleanupHolder::ReplicationSlot) => format!(
            "A replication slot with an xmin (a standby with hot_standby_feedback) keeps VACUUM from removing rows deleted or updated after it, in every database.{}",
            if t.state.as_deref() == Some("inactive slot") {
                " It is inactive: the standby that used it is gone or disconnected, so it never moves on."
            } else {
                ""
            }
        ),
        (Engine::Postgres, CleanupHolder::PreparedTransaction) => format!(
            "A prepared transaction (two-phase commit) keeps its locks and VACUUM's horizon until someone commits or rolls it back, and it survives restarts.{behind_note}{}",
            t.program.as_deref().map(|o| format!(" It was prepared by {o}.")).unwrap_or_default()
        ),
        (Engine::Postgres, CleanupHolder::Session) => format!(
            "PostgreSQL can't remove rows deleted or updated after this transaction's snapshot, so tables and indexes bloat and scans read dead rows.{behind_note}{}{program}",
            if idle { " This one is idle: the application opened it and never committed." } else { "" }
        ),
        (Engine::Mysql, _) => {
            let history = signal(snapshot, "history-length")
                .filter(|h| *h > 0.0)
                .map(|h| {
                    format!(" InnoDB's undo history (the whole server's) is {} undo logs long now.", format::count(h))
                })
                .unwrap_or_default();
            format!(
                "InnoDB keeps old versions of changed rows until the oldest read view that might need them closes. Transaction age and isolation level do not establish whether this transaction has acquired that read view, or how much undo it retains. Inspect its read view and locks before attributing whole-server purge history to it.{history}{}{program}",
                other_database.map(|d| format!(" It runs in database {d}.")).unwrap_or_default()
            )
        }
        (Engine::Sqlite, _) => format!("It holds a lock on the file.{program}"),
    };
    f.severity = if t.seconds >= 3.0 * 3600.0 || held_gb >= 10.0 || (slot && behind >= SLOT_FAR_BEHIND) {
        Severity::High
    } else {
        Severity::Medium
    };
    f.impact.score = base_score(f.severity);
    f.impact.label =
        if slot { format!("{} transactions behind", format::count(behind)) } else { format!("open {open}") };
    f.confidence = if engine == Engine::Mysql { Confidence::Medium } else { Confidence::High };
    if !slot {
        f.metrics.push(Metric::new("Open for", t.seconds * 1000.0, Unit::Ms));
    }
    if let Some(b) = t.held_bytes.filter(|b| *b > 0.0) {
        f.metrics.push(Metric::new("Version store", b, Unit::Bytes));
    }
    if behind > 0.0 {
        f.metrics.push(Metric::new("Transactions behind", behind, Unit::Count));
    }
    f.fix = Some(match holder {
        CleanupHolder::ReplicationSlot => Fix {
            summary: "Identify the slot's physical standby or logical-decoding consumer and restore its progress. Confirm whether it is intentionally retained before any removal; removing a slot can require rebuilding the consumer and lose its ability to resume.".into(),
            sql: None,
            cost: Some("Preserve a slot needed by a consumer until recovery or replacement is planned.".into()),
        },
        CleanupHolder::PreparedTransaction => Fix {
            summary: "Find what prepared it (the transaction manager or application doing two-phase commit) and have it commit or roll back. If nothing will, a DBA can roll it back.".into(),
            sql: Some(format!("ROLLBACK PREPARED '{}';", t.session.replace('\'', "''"))),
            cost: Some("Rolling it back discards its changes.".into()),
        },
        CleanupHolder::Session => {
            let end = engine.dialect().end_session(&t.session);
            Fix {
                summary: "Find out what the application does in that session and make it commit or roll back promptly; transactions should never wait on a person or a slow call. After checking it is safe, a DBA can end the session, which rolls its transaction back.".into(),
                sql: Some(format!("-- Ends the session and rolls back its open transaction\n{end}")),
                cost: Some("Ending the session discards that transaction's uncommitted changes.".into()),
            }
        }
    });
    f.verify = vec![if slot {
        "Re-scan: no replication slot should hold VACUUM back by millions of transactions.".into()
    } else {
        "Re-scan: the oldest open transaction should be minutes old, not hours.".into()
    }];
    vec![f]
}
