//! What blocked sessions wait for, and the latest deadlock InnoDB remembers (`SHOW ENGINE INNODB STATUS`, which needs
//! PROCESS). InnoDB prints the locked rows' contents too; Culprit keeps only tables, indexes and lock modes.

use std::collections::{HashMap, HashSet};

use culprit_core::deadlock::classify;
use culprit_core::{Deadlock, DeadlockProcess, DeadlockReport, DeadlockResource, Engine, LockHolder, LockWait, Result};
use mysql_async::Conn;

use crate::connect::Flavor;
use crate::rows::{Rec, query, query_one};

/// MySQL 8.0+: performance_schema's lock views.
const LOCK_WAITS_SQL: &str = "SELECT t.PROCESSLIST_ID AS session, b.PROCESSLIST_ID AS blocking, rl.LOCK_MODE AS mode,
       rl.LOCK_TYPE AS resource, CONCAT(rl.OBJECT_SCHEMA, '.', rl.OBJECT_NAME) AS object, rl.INDEX_NAME AS index_name
  FROM performance_schema.data_lock_waits w
  JOIN performance_schema.data_locks rl ON rl.ENGINE_LOCK_ID = w.REQUESTING_ENGINE_LOCK_ID
  JOIN performance_schema.threads t ON t.THREAD_ID = w.REQUESTING_THREAD_ID
  JOIN performance_schema.threads b ON b.THREAD_ID = w.BLOCKING_THREAD_ID";

/// MariaDB (and MySQL 5.7): InnoDB's own lock views in information_schema, which need PROCESS.
const INNODB_LOCK_WAITS_SQL: &str = "SELECT r.trx_mysql_thread_id AS session, b.trx_mysql_thread_id AS blocking,
       l.lock_mode AS mode, l.lock_type AS resource, REPLACE(l.lock_table, '`', '') AS object, l.lock_index AS index_name
  FROM information_schema.INNODB_LOCK_WAITS w
  JOIN information_schema.INNODB_TRX r ON r.trx_id = w.requesting_trx_id
  JOIN information_schema.INNODB_TRX b ON b.trx_id = w.blocking_trx_id
  LEFT JOIN information_schema.INNODB_LOCKS l ON l.lock_id = w.requested_lock_id";

/// Every (waiting, blocking) pair with what the waiter asked for, from whichever lock views this server has.
async fn wait_pairs(conn: &mut Conn, flavor: Flavor) -> Result<Vec<Rec>> {
    match flavor {
        Flavor::Mariadb => query(conn, INNODB_LOCK_WAITS_SQL).await,
        Flavor::Mysql => match query(conn, LOCK_WAITS_SQL).await {
            Ok(rows) => Ok(rows),
            Err(e) => query(conn, INNODB_LOCK_WAITS_SQL).await.map_err(|_| e),
        },
    }
}

/// A waiter queued behind several sessions is reported once per session ahead of it. The one it waits on directly is
/// the session that is itself waiting (next in the queue) if there is one, so chains read in queue order.
fn direct_blockers(pairs: &[(String, String)]) -> HashMap<String, String> {
    let waiting: HashSet<&str> = pairs.iter().map(|(session, _)| session.as_str()).collect();
    let mut out: HashMap<String, String> = HashMap::new();
    for (session, blocking) in pairs {
        if session.is_empty() || blocking.is_empty() || session == blocking {
            continue;
        }
        match out.get(session) {
            Some(current) if waiting.contains(current.as_str()) => {}
            _ => {
                out.insert(session.clone(), blocking.clone());
            }
        }
    }
    out
}

/// Which session blocks which. Empty when the login can't see InnoDB's locks.
pub(crate) async fn blockers(conn: &mut Conn, flavor: Flavor) -> HashMap<String, String> {
    let pairs: Vec<(String, String)> = wait_pairs(conn, flavor)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| (r.string("session"), r.string("blocking")))
        .collect();
    direct_blockers(&pairs)
}

pub(crate) async fn lock_waits(conn: &mut Conn, flavor: Flavor) -> Result<Vec<LockWait>> {
    let mut out: Vec<LockWait> = Vec::new();
    for r in wait_pairs(conn, flavor).await? {
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

/// The most recent statement of each sleeping connection (performance_schema keeps it after it finished). MariaDB
/// turns the current-statement consumer off by default; the per-thread history, when on, has it too.
pub(crate) async fn last_statements(conn: &mut Conn) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for sql in [
        "SELECT t.PROCESSLIST_ID AS id, s.SQL_TEXT AS sql_text
           FROM performance_schema.events_statements_current s
           JOIN performance_schema.threads t ON t.THREAD_ID = s.THREAD_ID
          WHERE t.PROCESSLIST_COMMAND = 'Sleep' AND s.SQL_TEXT IS NOT NULL",
        "SELECT t.PROCESSLIST_ID AS id, s.SQL_TEXT AS sql_text
           FROM performance_schema.events_statements_history s
           JOIN performance_schema.threads t ON t.THREAD_ID = s.THREAD_ID
          WHERE t.PROCESSLIST_COMMAND = 'Sleep' AND s.SQL_TEXT IS NOT NULL
          ORDER BY s.THREAD_ID, s.EVENT_ID DESC",
    ] {
        for r in query(conn, sql).await.unwrap_or_default() {
            if let (Some(id), Some(text)) =
                (r.text("id"), r.text("sql_text").filter(|t| !t.contains(crate::connect::TAG)))
            {
                out.entry(id).or_insert(text);
            }
        }
    }
    out
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
    /// InnoDB's transaction id, which lock lines name (`trx id 76`).
    trx: Option<String>,
    thread: Option<String>,
    host: Option<String>,
    user: Option<String>,
    statement: Vec<String>,
}

/// The LATEST DETECTED DEADLOCK section of `SHOW ENGINE INNODB STATUS` (or the whole output). `utc` says whether the
/// server prints times in UTC; otherwise the time is left out rather than guessed.
///
/// MySQL lists, per transaction, the locks it HOLDS and the one it is WAITING FOR. MariaDB lists the lock each one is
/// WAITING FOR and the locks it is CONFLICTING WITH, which belong to the other transaction: every lock line names its
/// owner (`trx id 76`), so holders are attributed by that, whichever format it is.
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
    let mut pending_record: Option<Pending> = None;

    let flush = |pending: &mut Option<Pending>,
                 heap: Option<&str>,
                 current: &Option<String>,
                 holding: Option<bool>,
                 txns: &[Txn],
                 resources: &mut Vec<DeadlockResource>| {
        let Some(Pending { object, index, page, mode, owner }) = pending.take() else { return };
        // The owner may be a transaction listed further down (MariaDB's CONFLICTING WITH): name it by its InnoDB id
        // until every transaction is known.
        let by_owner = owner.as_ref().map(|trx| {
            txns.iter()
                .find(|t| t.trx.as_deref() == Some(trx.as_str()))
                .map(|t| t.id.clone())
                .unwrap_or(format!("trx:{trx}"))
        });
        let Some(process) = by_owner.or_else(|| current.clone()) else { return };
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
        if t.starts_with("*** WAITING FOR THIS LOCK") || t.starts_with("*** CONFLICTING WITH") {
            flush(&mut pending_record, None, &current, holding, &txns, &mut resources);
            holding = Some(t.starts_with("*** CONFLICTING"));
            continue;
        }
        if let Some(rest) = t.strip_prefix("*** (").and_then(|r| r.split_once(')')) {
            flush(&mut pending_record, None, &current, holding, &txns, &mut resources);
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
            flush(&mut pending_record, None, &current, holding, &txns, &mut resources);
            victim = rest.split(')').next().map(|n| format!("({n})"));
            continue;
        }
        if holding.is_none() {
            let Some(txn) = txns.last_mut() else { continue };
            if let Some(rest) = t.strip_prefix("TRANSACTION ")
                && txn.trx.is_none()
            {
                txn.trx = rest.split(',').next().map(|s| s.trim().to_string());
                continue;
            }
            if let Some(rest) = t.strip_prefix("MySQL thread id ").or_else(|| t.strip_prefix("MariaDB thread id ")) {
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
            flush(&mut pending_record, None, &current, holding, &txns, &mut resources);
            let owner = t.split("trx id ").nth(1).and_then(|r| r.split_whitespace().next()).map(str::to_string);
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
                pending_record = Some(Pending { object, index: "TABLE".into(), page: String::new(), mode, owner });
                flush(&mut pending_record, None, &current, holding, &txns, &mut resources);
            } else {
                let index =
                    t.split(" index ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or("").to_string();
                let page = t
                    .split("space id ")
                    .nth(1)
                    .map(|r| r.split(" n bits").next().unwrap_or(r).replace(" page no ", ":"))
                    .unwrap_or_default();
                pending_record = Some(Pending { object, index, page, mode, owner });
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Record lock, heap no ") {
            let heap = rest.split_whitespace().next();
            flush(&mut pending_record, heap, &current, holding, &txns, &mut resources);
        }
        // Everything else is the record's contents, which Culprit doesn't keep.
    }
    flush(&mut pending_record, None, &current, holding, &txns, &mut resources);
    for entry in resources.iter_mut().flat_map(|r| r.holders.iter_mut().chain(r.waiters.iter_mut())) {
        if let Some(trx) = entry.process.strip_prefix("trx:")
            && let Some(t) = txns.iter().find(|t| t.trx.as_deref() == Some(trx))
        {
            entry.process = t.id.clone();
        }
    }

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

/// A lock line waiting for the record line that says which row (heap number) it is.
struct Pending {
    object: String,
    index: String,
    page: String,
    mode: String,
    /// The transaction id the line names as the lock's owner.
    owner: Option<String>,
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

    /// MariaDB's layout: each transaction lists the lock it waits for and the locks of the other one it conflicts with.
    const MARIADB_STATUS: &str = "------------------------
LATEST DETECTED DEADLOCK
------------------------
2026-10-11 15:28:06 0xffff807aefe0
*** (1) TRANSACTION:
TRANSACTION 76, ACTIVE 2 sec starting index read
mysql tables in use 1, locked 1
LOCK WAIT 4 lock struct(s), heap size 1120, 2 row lock(s)
MariaDB thread id 45, OS thread handle 281472837283808, query id 40423 localhost app Updating
UPDATE customers SET name = name WHERE id = 1
*** WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 10 page no 5 n bits 320 index PRIMARY of table `shop`.`customers` trx id 76 lock_mode X locks rec but not gap waiting
Record lock, heap no 2 PHYSICAL RECORD: n_fields 7; compact format; info bits 0
 3: len 21; hex 637573746f6d657230406578616d706c652e636f6d; asc customer0@example.com;;

*** CONFLICTING WITH:
RECORD LOCKS space id 10 page no 5 n bits 320 index PRIMARY of table `shop`.`customers` trx id 75 lock_mode X locks rec but not gap
Record lock, heap no 2 PHYSICAL RECORD: n_fields 7; compact format; info bits 0
 3: len 21; hex 637573746f6d657230406578616d706c652e636f6d; asc customer0@example.com;;


*** (2) TRANSACTION:
TRANSACTION 75, ACTIVE 2 sec starting index read
mysql tables in use 1, locked 1
LOCK WAIT 4 lock struct(s), heap size 1120, 2 row lock(s)
MariaDB thread id 44, OS thread handle 281472964333536, query id 40422 localhost app Updating
UPDATE orders SET status = status WHERE id = 1
*** WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 12 page no 7 n bits 320 index PRIMARY of table `shop`.`orders` trx id 75 lock_mode X locks rec but not gap waiting
Record lock, heap no 2 PHYSICAL RECORD: n_fields 10; compact format; info bits 0
 4: len 7; hex 70656e64696e67; asc pending;;

*** CONFLICTING WITH:
RECORD LOCKS space id 12 page no 7 n bits 320 index PRIMARY of table `shop`.`orders` trx id 76 lock_mode X locks rec but not gap
Record lock, heap no 2 PHYSICAL RECORD: n_fields 10; compact format; info bits 0
 4: len 7; hex 70656e64696e67; asc pending;;

*** WE ROLL BACK TRANSACTION (1)
------------
TRANSACTIONS
------------
";

    #[test]
    fn mariadb_deadlocks_attribute_held_locks_by_transaction_id() {
        let found = parse_innodb_status(MARIADB_STATUS, true);
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.pattern, DeadlockPattern::OppositeOrder);
        assert_eq!(d.objects, vec!["shop.customers".to_string(), "shop.orders".to_string()]);
        let customers = d.resources.iter().find(|r| r.object.as_deref() == Some("shop.customers")).unwrap();
        // Transaction 75 is (2): it holds the customer row that (1) waits for.
        assert_eq!(customers.holders[0].process, "(2)");
        assert_eq!(customers.waiters[0].process, "(1)");
        let victim = d.victim().unwrap();
        assert_eq!(
            (victim.session.as_deref(), victim.statement.as_deref()),
            (Some("45"), Some("UPDATE customers SET name = name WHERE id = 1"))
        );
        assert!(!serde_json::to_string(d).unwrap().contains("customer0@example.com"));
    }

    #[test]
    fn queued_waiters_point_at_the_session_ahead_of_them() {
        let pairs: Vec<(String, String)> =
            [("79", "78"), ("79", "77"), ("78", "77")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let direct = direct_blockers(&pairs);
        assert_eq!(direct.get("79").map(String::as_str), Some("78"), "79 waits behind 78, which waits behind 77");
        assert_eq!(direct.get("78").map(String::as_str), Some("77"));
    }

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
