//! What blocked sessions wait for, and deadlocks. Postgres counts deadlocks per database but writes the details only to
//! the server log, which SQL can't read; Culprit shows the counter and explains pasted log lines.

use culprit_core::deadlock::classify;
use culprit_core::{Deadlock, DeadlockProcess, DeadlockReport, DeadlockResource, Engine, LockHolder, LockWait, Result};
use tokio_postgres::Client;

use crate::rows::{iso, query, query_one};

/// The lock each waiting backend asked for. A row-lock wait is a wait on another transaction's id; the table comes from
/// the tuple lock the waiter holds (or queues on), or else the table it is changing.
const LOCK_WAITS_SQL: &str = "
SELECT w.pid::text AS session, w.mode, w.locktype,
       coalesce(rel.name, tup.name, mod.name) AS object
  FROM pg_locks w
  LEFT JOIN LATERAL (SELECT n.nspname || '.' || c.relname AS name FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE c.oid = w.relation) rel ON true
  LEFT JOIN LATERAL (SELECT n.nspname || '.' || c.relname AS name FROM pg_locks t
                       JOIN pg_class c ON c.oid = t.relation JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE t.pid = w.pid AND t.locktype = 'tuple' LIMIT 1) tup ON true
  LEFT JOIN LATERAL (SELECT n.nspname || '.' || c.relname AS name FROM pg_locks t
                       JOIN pg_class c ON c.oid = t.relation JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE t.pid = w.pid AND t.locktype = 'relation' AND t.mode = 'RowExclusiveLock' AND c.relkind IN ('r', 'p')
                        AND n.nspname NOT IN ('pg_catalog', 'information_schema') LIMIT 1) mod ON true
 WHERE NOT w.granted AND w.pid <> pg_backend_pid()";

pub(crate) async fn lock_waits(client: &Client) -> Result<Vec<LockWait>> {
    let mut out: Vec<LockWait> = Vec::new();
    for r in query(client, LOCK_WAITS_SQL).await? {
        let session = r.string("session");
        if out.iter().any(|l| l.session == session) {
            continue;
        }
        out.push(LockWait {
            session,
            mode: r.text("mode"),
            resource: r.text("locktype"),
            object: r.text("object"),
            index: None,
        });
    }
    Ok(out)
}

pub(crate) async fn recent(client: &Client, since_hours: f64) -> Result<DeadlockReport> {
    let row = query_one(
        client,
        &format!(
            "SELECT deadlocks, {} AS since, current_setting('log_lock_waits') AS log_lock_waits
               FROM pg_stat_database WHERE datname = current_database()",
            iso("stats_reset")
        ),
    )
    .await?;
    let counted = row.as_ref().and_then(|r| r.number("deadlocks"));
    let since = row.as_ref().and_then(|r| r.text("since"));
    let log_waits = row.as_ref().and_then(|r| r.text("log_lock_waits")).is_some_and(|v| v == "on");
    let when = since
        .as_deref()
        .map(|s| format!(" since statistics were reset ({})", &s[..s.len().min(10)]))
        .unwrap_or_default();
    let note = format!(
        "Postgres counted {} deadlock{} in this database{when}. It writes the details (who waited for whom, and the \
         statements) only to the server log, which SQL can't read: paste those log lines here to have them explained.{}",
        counted.map(|c| format!("{c:.0}")).unwrap_or_else(|| "no".into()),
        if counted == Some(1.0) { "" } else { "s" },
        if log_waits { "" } else { " Turning on log_lock_waits also logs any lock wait longer than deadlock_timeout." }
    );
    Ok(DeadlockReport {
        deadlocks: Vec::new(),
        source: "pg_stat_database".into(),
        since_hours,
        counted,
        counted_since: since,
        note: Some(note),
    })
}

/// Deadlocks in pasted Postgres server log text (`ERROR: deadlock detected` with its DETAIL and CONTEXT lines).
pub fn parse_deadlock_log(text: &str) -> Vec<Deadlock> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (start, line) in lines.iter().enumerate() {
        if !line.contains("deadlock detected") {
            continue;
        }
        let reporter = bracketed_pid(line);
        let at = log_time(line);
        let block: Vec<&str> =
            lines[start + 1..].iter().take_while(|l| !l.contains("deadlock detected")).take(40).copied().collect();
        let mut processes: Vec<DeadlockProcess> = Vec::new();
        let mut resources: Vec<DeadlockResource> = Vec::new();
        let mut context_relation: Option<String> = None;
        for raw in &block {
            let l = raw.split_once("DETAIL:").map(|(_, r)| r).unwrap_or(raw).trim();
            if let Some(rest) = raw.split_once("CONTEXT:").map(|(_, r)| r) {
                context_relation =
                    rest.split("relation \"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string);
                continue;
            }
            // Process 539 waits for ShareLock on transaction 838; blocked by process 545.
            if let Some(rest) = l.strip_prefix("Process ")
                && let Some((pid, rest)) = rest.split_once(" waits for ")
                && let Some((mode, rest)) = rest.split_once(" on ")
                && let Some((what, blocker)) = rest.split_once("; blocked by process ")
            {
                let blocker = blocker.trim_end_matches('.').trim().to_string();
                let kind = what.split_whitespace().next().unwrap_or("lock").to_string();
                resources.push(DeadlockResource {
                    id: what.to_string(),
                    kind,
                    object: None,
                    index: None,
                    holders: vec![LockHolder { process: blocker, mode: None }],
                    waiters: vec![LockHolder { process: pid.to_string(), mode: Some(mode.to_string()) }],
                });
                if !processes.iter().any(|p| p.id == pid) {
                    processes.push(DeadlockProcess {
                        id: pid.into(),
                        session: Some(pid.into()),
                        wait_mode: Some(mode.into()),
                        ..Default::default()
                    });
                }
                continue;
            }
            // Process 539: UPDATE …
            if let Some(rest) = l.strip_prefix("Process ")
                && let Some((pid, statement)) = rest.split_once(": ")
                && pid.chars().all(|c| c.is_ascii_digit())
                && let Some(p) = processes.iter_mut().find(|p| p.id == pid)
            {
                p.statement = Some(statement.trim().to_string());
            }
        }
        if processes.is_empty() {
            continue;
        }
        // The process that logged the error is the one Postgres cancelled.
        for p in &mut processes {
            p.victim = reporter.as_deref() == Some(p.id.as_str());
        }
        if let (Some(relation), Some(victim)) = (context_relation, reporter.as_deref()) {
            for r in resources.iter_mut().filter(|r| r.waiters.iter().any(|w| w.process == victim)) {
                r.object = Some(relation.clone());
            }
        }
        let mut d = Deadlock::new("pasted log", at, processes, resources);
        classify(Engine::Postgres, &mut d);
        out.push(d);
    }
    out
}

fn bracketed_pid(line: &str) -> Option<String> {
    let inside = line.split_once('[')?.1.split_once(']')?.0;
    inside.chars().all(|c| c.is_ascii_digit()).then(|| inside.to_string())
}

/// `2026-10-11 13:48:28.887 UTC [539] ERROR: …` → `2026-10-11T13:48:28Z` (only when the log says UTC).
fn log_time(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    let (date, time, zone) = (parts.next()?, parts.next()?, parts.next()?);
    let looks_like_date = date.len() == 10 && date.chars().filter(|c| *c == '-').count() == 2;
    (looks_like_date && matches!(zone, "UTC" | "GMT"))
        .then(|| format!("{date}T{}Z", time.split('.').next().unwrap_or(time)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::DeadlockPattern;

    pub(crate) const LOG: &str = "2026-10-11 13:48:28.887 UTC [539] ERROR:  deadlock detected
2026-10-11 13:48:28.887 UTC [539] DETAIL:  Process 539 waits for ShareLock on transaction 838; blocked by process 545.
\tProcess 545 waits for ShareLock on transaction 837; blocked by process 539.
\tProcess 539: UPDATE customers SET id = id WHERE id = 1
\tProcess 545: UPDATE orders SET id = id WHERE id = 1
2026-10-11 13:48:28.887 UTC [539] HINT:  See server log for query details.
2026-10-11 13:48:28.887 UTC [539] CONTEXT:  while updating tuple (0,1) in relation \"customers\"
2026-10-11 13:48:28.887 UTC [539] STATEMENT:  UPDATE customers SET id = id WHERE id = 1";

    #[test]
    fn a_pasted_log_deadlock_is_explained() {
        let found = parse_deadlock_log(LOG);
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.at.as_deref(), Some("2026-10-11T13:48:28Z"));
        assert_eq!(d.processes.len(), 2);
        assert_eq!(d.victim().unwrap().id, "539");
        assert_eq!(d.processes[1].statement.as_deref(), Some("UPDATE orders SET id = id WHERE id = 1"));
        assert_eq!(d.pattern, DeadlockPattern::OppositeOrder);
        assert_eq!(d.objects, vec!["customers".to_string()]);
        assert!(d.fixes.iter().any(|f| f.detail.contains("40P01")));
        assert!(parse_deadlock_log("nothing here").is_empty());
    }
}
