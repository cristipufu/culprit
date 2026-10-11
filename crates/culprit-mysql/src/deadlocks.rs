//! What blocked sessions wait for, and the latest deadlock InnoDB remembers (`SHOW ENGINE INNODB STATUS`, which needs
//! PROCESS). InnoDB prints the locked rows' contents too; Culprit keeps only tables, indexes and lock modes.

use culprit_core::deadlock::classify;
use culprit_core::{Deadlock, DeadlockProcess, DeadlockReport, DeadlockResource, Engine, LockHolder, LockWait, Result};
use mysql_async::Conn;

use crate::rows::{query, query_one};

const LOCK_WAITS_SQL: &str = "SELECT t.PROCESSLIST_ID AS session, rl.LOCK_MODE AS mode, rl.LOCK_TYPE AS resource,
       CONCAT(rl.OBJECT_SCHEMA, '.', rl.OBJECT_NAME) AS object, rl.INDEX_NAME AS index_name
  FROM performance_schema.data_lock_waits w
  JOIN performance_schema.data_locks rl ON rl.ENGINE_LOCK_ID = w.REQUESTING_ENGINE_LOCK_ID
  JOIN performance_schema.threads t ON t.THREAD_ID = w.REQUESTING_THREAD_ID";

pub(crate) async fn lock_waits(conn: &mut Conn) -> Result<Vec<LockWait>> {
    let mut out: Vec<LockWait> = Vec::new();
    for r in query(conn, LOCK_WAITS_SQL).await? {
        let session = r.string("session");
        if out.iter().any(|l| l.session == session) {
            continue;
        }
        out.push(LockWait {
            session,
            mode: r.text("mode"),
            resource: r.text("resource"),
            object: r.text("object"),
            index: r.text("index_name"),
        });
    }
    Ok(out)
}

/// The most recent statement of each sleeping connection (performance_schema keeps it after it finished).
pub(crate) async fn last_statements(conn: &mut Conn) -> std::collections::HashMap<String, String> {
    query(
        conn,
        "SELECT t.PROCESSLIST_ID AS id, s.SQL_TEXT AS sql_text
           FROM performance_schema.events_statements_current s
           JOIN performance_schema.threads t ON t.THREAD_ID = s.THREAD_ID
          WHERE t.PROCESSLIST_COMMAND = 'Sleep' AND s.SQL_TEXT IS NOT NULL",
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| Some((r.text("id")?, r.text("sql_text")?)))
    .collect()
}

pub(crate) async fn recent(conn: &mut Conn, since_hours: f64) -> Result<DeadlockReport> {
    let mut report = DeadlockReport { source: "InnoDB status".into(), since_hours, ..Default::default() };
    let status = match query_one(conn, "SHOW ENGINE INNODB STATUS").await {
        Ok(row) => row.and_then(|r| r.text("Status").or_else(|| r.text("status"))).unwrap_or_default(),
        Err(_) => {
            report.note = Some(
                "Reading InnoDB's deadlock report needs the PROCESS privilege. Paste the LATEST DETECTED DEADLOCK section \
                 of SHOW ENGINE INNODB STATUS to have it explained."
                    .into(),
            );
            return Ok(report);
        }
    };
    let utc = query_one(conn, "SELECT @@system_time_zone AS tz")
        .await
        .ok()
        .flatten()
        .and_then(|r| r.text("tz"))
        .is_some_and(|tz| matches!(tz.to_ascii_uppercase().as_str(), "UTC" | "GMT" | "+00:00" | "Z" | "ETC/UTC"));
    // RFC 3339 UTC times compare as text.
    let cutoff = query_one(
        conn,
        &format!(
            "SELECT DATE_FORMAT(UTC_TIMESTAMP() - INTERVAL {} SECOND, '%Y-%m-%dT%H:%i:%sZ') AS cutoff",
            (since_hours * 3600.0).round() as i64
        ),
    )
    .await
    .ok()
    .flatten()
    .and_then(|r| r.text("cutoff"));
    let mut found = parse_innodb_status(&status, utc);
    found.retain(|d| match (d.at.as_deref(), cutoff.as_deref()) {
        (Some(at), Some(cutoff)) => at >= cutoff,
        _ => true,
    });
    report.deadlocks = found;
    report.note = Some(
        "InnoDB keeps only its most recent deadlock. Turn on innodb_print_all_deadlocks to write every one to the error log."
            .into(),
    );
    Ok(report)
}

#[derive(Default)]
struct Txn {
    id: String,
    thread: Option<String>,
    host: Option<String>,
    user: Option<String>,
    statement: Vec<String>,
}

/// The LATEST DETECTED DEADLOCK section of `SHOW ENGINE INNODB STATUS` (or the whole output). `utc` says whether the
/// server prints times in UTC; otherwise the time is left out rather than guessed.
pub fn parse_innodb_status(text: &str, utc: bool) -> Vec<Deadlock> {
    let Some(start) = text.find("LATEST DETECTED DEADLOCK") else { return Vec::new() };
    let section = &text[start..];
    let end = section.find("\nTRANSACTIONS\n").unwrap_or(section.len());
    let lines: Vec<&str> = section[..end].lines().skip(1).collect();

    let at = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty() && !l.starts_with('-')).and_then(|l| {
        let mut p = l.split_whitespace();
        let (date, time) = (p.next()?, p.next()?);
        (utc && date.len() == 10 && date.contains('-')).then(|| format!("{date}T{time}Z"))
    });

    let mut txns: Vec<Txn> = Vec::new();
    let mut resources: Vec<DeadlockResource> = Vec::new();
    let mut victim: Option<String> = None;
    let mut current: Option<String> = None;
    // Some(true): lock lines that follow are held; Some(false): waited for; None: transaction header.
    let mut holding: Option<bool> = None;
    let mut in_statement = false;
    let mut pending_record: Option<(String, String, String, String)> = None; // (object, index, page, mode)

    let flush = |pending: &mut Option<(String, String, String, String)>,
                 heap: Option<&str>,
                 current: &Option<String>,
                 holding: Option<bool>,
                 resources: &mut Vec<DeadlockResource>| {
        let Some((object, index, page, mode)) = pending.take() else { return };
        let Some(process) = current.clone() else { return };
        let id = format!("{object}/{index}/{page}/{}", heap.unwrap_or("?"));
        let r = match resources.iter_mut().position(|r| r.id == id) {
            Some(i) => &mut resources[i],
            None => {
                resources.push(DeadlockResource {
                    id: id.clone(),
                    kind: if index == "TABLE" { "TABLE".into() } else { "RECORD".into() },
                    object: Some(object),
                    index: (index != "TABLE").then_some(index),
                    holders: vec![],
                    waiters: vec![],
                });
                resources.last_mut().unwrap()
            }
        };
        let entry = LockHolder { process, mode: Some(mode) };
        if holding == Some(false) { r.waiters.push(entry) } else { r.holders.push(entry) }
    };

    for raw in lines {
        let line = raw.trim_end();
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("*** (").and_then(|r| r.split_once(')')) {
            flush(&mut pending_record, None, &current, holding, &mut resources);
            let (n, tail) = rest;
            let id = format!("({n})");
            if tail.contains("TRANSACTION:") {
                txns.push(Txn { id: id.clone(), ..Default::default() });
                holding = None;
                in_statement = false;
            } else if tail.contains("HOLDS THE LOCK") {
                holding = Some(true);
            } else if tail.contains("WAITING FOR THIS LOCK") {
                holding = Some(false);
            }
            current = Some(id);
            continue;
        }
        if let Some(rest) = t.strip_prefix("*** WE ROLL BACK TRANSACTION (") {
            flush(&mut pending_record, None, &current, holding, &mut resources);
            victim = rest.split(')').next().map(|n| format!("({n})"));
            continue;
        }
        if holding.is_none() {
            let Some(txn) = txns.last_mut() else { continue };
            if let Some(rest) = t.strip_prefix("MySQL thread id ") {
                txn.thread = rest.split(',').next().map(|s| s.trim().to_string());
                // "…, query id 40650 localhost root updating"
                if let Some(after) = rest.split("query id ").nth(1) {
                    let mut parts = after.split_whitespace().skip(1);
                    txn.host = parts.next().map(str::to_string);
                    txn.user = parts.next().map(str::to_string);
                }
                in_statement = true;
                continue;
            }
            if in_statement {
                if t.is_empty() {
                    in_statement = false;
                } else {
                    txn.statement.push(t.to_string());
                }
            }
            continue;
        }
        if t.starts_with("RECORD LOCKS ") || t.starts_with("TABLE LOCK ") {
            flush(&mut pending_record, None, &current, holding, &mut resources);
            let object = t
                .split("table `")
                .nth(1)
                .map(|r| r.replace("`.`", ".").split('`').next().unwrap_or("").to_string())
                .unwrap_or_default();
            let mode = t
                .split("lock_mode ")
                .nth(1)
                .or_else(|| t.split("lock mode ").nth(1))
                .map(|m| mode_label(m.trim_end_matches(" waiting").trim()))
                .unwrap_or_default();
            if t.starts_with("TABLE LOCK ") {
                pending_record = Some((object, "TABLE".into(), String::new(), mode));
                flush(&mut pending_record, None, &current, holding, &mut resources);
            } else {
                let index =
                    t.split(" index ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or("").to_string();
                let page = t
                    .split("space id ")
                    .nth(1)
                    .map(|r| r.split(" n bits").next().unwrap_or(r).replace(" page no ", ":"))
                    .unwrap_or_default();
                pending_record = Some((object, index, page, mode));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Record lock, heap no ") {
            let heap = rest.split_whitespace().next();
            flush(&mut pending_record, heap, &current, holding, &mut resources);
        }
        // Everything else is the record's contents, which Culprit doesn't keep.
    }
    flush(&mut pending_record, None, &current, holding, &mut resources);

    if txns.is_empty() {
        return Vec::new();
    }
    let processes = txns
        .into_iter()
        .map(|t| DeadlockProcess {
            victim: victim.as_deref() == Some(t.id.as_str()),
            session: t.thread,
            statement: (!t.statement.is_empty()).then(|| t.statement.join(" ")),
            login: t.user,
            host: t.host,
            id: t.id,
            ..Default::default()
        })
        .collect();
    let mut d = Deadlock::new("InnoDB status", at, processes, resources);
    classify(Engine::Mysql, &mut d);
    vec![d]
}

/// `X locks rec but not gap` → `X (row)`; `X` → `X (row and gap)`; `IX` stays.
fn mode_label(raw: &str) -> String {
    let mut parts = raw.splitn(2, ' ');
    let mode = parts.next().unwrap_or("").to_string();
    let rest = parts.next().unwrap_or("");
    let kind = if rest.contains("rec but not gap") {
        " (row)"
    } else if rest.contains("gap before rec") {
        " (gap)"
    } else if rest.contains("insert intention") {
        " (insert intention)"
    } else if rest.is_empty() && !mode.starts_with('I') {
        " (row and gap)"
    } else {
        ""
    };
    format!("{mode}{kind}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::DeadlockPattern;

    pub(crate) const STATUS: &str = "=====================================
------------------------
LATEST DETECTED DEADLOCK
------------------------
2026-10-11 13:48:08 281472761716480
*** (1) TRANSACTION:
TRANSACTION 2321, ACTIVE 1 sec starting index read
mysql tables in use 1, locked 1
LOCK WAIT 4 lock struct(s), heap size 1128, 2 row lock(s)
MySQL thread id 74, OS thread handle 281472902020864, query id 40650 web-1 app updating
UPDATE customers SET id = id WHERE id = 1

*** (1) HOLDS THE LOCK(S):
RECORD LOCKS space id 6 page no 8 n bits 360 index PRIMARY of table `shop`.`orders` trx id 2321 lock_mode X locks rec but not gap
Record lock, heap no 2 PHYSICAL RECORD: n_fields 10; compact format; info bits 0
 0: len 4; hex 80000001; asc     ;;
 4: len 7; hex 70656e64696e67; asc pending;;


*** (1) WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 4 page no 6 n bits 280 index PRIMARY of table `shop`.`customers` trx id 2321 lock_mode X locks rec but not gap waiting
Record lock, heap no 2 PHYSICAL RECORD: n_fields 7; compact format; info bits 0
 3: len 21; hex 637573746f6d657230406578616d706c652e636f6d; asc customer0@example.com;;


*** (2) TRANSACTION:
TRANSACTION 2322, ACTIVE 1 sec starting index read
mysql tables in use 1, locked 1
LOCK WAIT 4 lock struct(s), heap size 1128, 2 row lock(s)
MySQL thread id 75, OS thread handle 281472904257280, query id 40651 web-2 app updating
UPDATE orders SET id = id WHERE id = 1

*** (2) HOLDS THE LOCK(S):
RECORD LOCKS space id 4 page no 6 n bits 280 index PRIMARY of table `shop`.`customers` trx id 2322 lock_mode X locks rec but not gap
Record lock, heap no 2 PHYSICAL RECORD: n_fields 7; compact format; info bits 0
 3: len 21; hex 637573746f6d657230406578616d706c652e636f6d; asc customer0@example.com;;


*** (2) WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 6 page no 8 n bits 360 index PRIMARY of table `shop`.`orders` trx id 2322 lock_mode X locks rec but not gap waiting
Record lock, heap no 2 PHYSICAL RECORD: n_fields 10; compact format; info bits 0
 0: len 4; hex 80000001; asc     ;;

*** WE ROLL BACK TRANSACTION (2)
------------
TRANSACTIONS
------------
Trx id counter 2330
";

    #[test]
    fn innodb_status_becomes_an_explained_deadlock_without_row_contents() {
        let found = parse_innodb_status(STATUS, true);
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.at.as_deref(), Some("2026-10-11T13:48:08Z"));
        assert_eq!(d.pattern, DeadlockPattern::OppositeOrder);
        assert_eq!(d.objects, vec!["shop.customers".to_string(), "shop.orders".to_string()]);
        assert_eq!(d.resources.len(), 2);
        assert_eq!(d.resources[0].holders[0].mode.as_deref(), Some("X (row)"));
        let victim = d.victim().unwrap();
        assert_eq!(
            (victim.session.as_deref(), victim.statement.as_deref()),
            (Some("75"), Some("UPDATE orders SET id = id WHERE id = 1"))
        );
        assert_eq!((victim.host.as_deref(), victim.login.as_deref()), (Some("web-2"), Some("app")));
        let json = serde_json::to_string(d).unwrap();
        assert!(!json.contains("customer0@example.com"), "row contents must not be kept");
        assert!(parse_innodb_status(STATUS, false)[0].at.is_none());
        assert!(parse_innodb_status("no deadlock here", true).is_empty());
    }
}
