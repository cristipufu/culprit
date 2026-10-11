//! Blocking chains: who holds the locks everyone else waits for, why, and what to do about it. A pure function of the
//! sessions Live already reads plus the lock each waiter wants, so Live, the CLI and MCP tell the same story.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::format;
use crate::model::{Engine, Session};

/// The lock a waiting session asked for and didn't get.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct LockWait {
    /// The waiting session.
    pub session: String,
    /// The lock mode it asked for, as the engine names it: `X`, `U`, `S`, `ShareLock`, `X,REC_NOT_GAP`.
    pub mode: Option<String>,
    /// What is locked: `KEY`, `PAGE`, `OBJECT`, `transactionid`, `RECORD`, `TABLE`.
    pub resource: Option<String>,
    /// `schema.table`, when the engine can tell.
    pub object: Option<String>,
    pub index: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum HeadKind {
    /// Holds a transaction open and sends nothing: the classic cause.
    IdleInTransaction,
    /// Still running a statement while holding locks.
    Running,
    /// Connected, not running anything, no transaction age known.
    Idle,
    /// Not visible to this login (another database, or not permitted).
    Unknown,
}

/// One blocked session (or the head) and who waits behind it.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct BlockNode {
    pub session: String,
    /// The lock this session waits for; `None` for the head.
    pub lock: Option<LockWait>,
    /// That lock in words: "an update lock on shop.orders".
    pub waits_for: Option<String>,
    pub children: Vec<BlockNode>,
}

/// Everyone waiting, directly or through others, on one head blocker.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct BlockingChain {
    pub head: String,
    pub head_kind: HeadKind,
    /// How long the head has been idle (idle heads) or running (running heads).
    pub head_ms: Option<f64>,
    /// Age of the head's open transaction.
    pub transaction_ms: Option<f64>,
    /// Sessions waiting behind the head, directly or through others.
    pub waiters: u32,
    pub longest_wait_ms: Option<f64>,
    /// The wait most of the head's direct waiters report, e.g. `LCK_M_X` or `Lock: transactionid`.
    pub wait_type: Option<String>,
    /// The lock most of the head's direct waiters want.
    pub lock: Option<LockWait>,
    /// One sentence: who, for how long, how many wait, on what.
    pub headline: String,
    /// What this kind of head usually means and what to do, in plain language.
    pub advice: String,
    pub tree: BlockNode,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Blocking {
    /// Biggest chain first.
    pub chains: Vec<BlockingChain>,
    /// Sessions waiting on another session, across all chains.
    pub waiting: u32,
}

/// The blocking chains in `sessions`, with lock details from `locks` where the engine provided them. `None` when
/// nobody waits on anybody.
pub fn analyze(engine: Engine, sessions: &[Session], locks: &[LockWait]) -> Option<Blocking> {
    let by_id: HashMap<&str, &Session> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let lock_of: HashMap<&str, &LockWait> = locks.iter().map(|l| (l.session.as_str(), l)).collect();
    let wait_of: HashMap<&str, &str> =
        sessions.iter().filter_map(|s| Some((s.id.as_str(), s.wait.as_deref()?))).collect();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in sessions {
        if let Some(blocker) = s.blocked_by.as_deref().filter(|b| *b != s.id) {
            children.entry(blocker).or_default().push(&s.id);
        }
    }
    if children.is_empty() {
        return None;
    }

    // Heads hold locks and wait on nobody. A cycle (a deadlock being resolved right now) has no such session, so the
    // smallest id in it stands in as the head.
    let mut heads: Vec<&str> = children
        .keys()
        .copied()
        .filter(|id| by_id.get(id).and_then(|s| s.blocked_by.as_deref()).is_none_or(|b| b == *id))
        .collect();
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    for head in &heads {
        collect(head, &children, &mut placed);
    }
    for id in children.keys() {
        if !placed.contains(id) {
            heads.push(id);
            collect(id, &children, &mut placed);
        }
    }

    let mut chains: Vec<BlockingChain> = heads
        .into_iter()
        .map(|head| {
            let mut seen = BTreeSet::new();
            let tree = node(head, &children, &lock_of, &wait_of, &mut seen, true);
            chain(engine, head, by_id.get(head).copied(), tree, &by_id, &lock_of)
        })
        .filter(|c| c.waiters > 0)
        .collect();
    chains.sort_by(|a, b| b.waiters.cmp(&a.waiters).then_with(|| a.head.cmp(&b.head)));
    let waiting = chains.iter().map(|c| c.waiters).sum();
    Some(Blocking { chains, waiting })
}

fn collect<'a>(id: &'a str, children: &BTreeMap<&'a str, Vec<&'a str>>, placed: &mut BTreeSet<&'a str>) {
    if !placed.insert(id) {
        return;
    }
    for child in children.get(id).into_iter().flatten() {
        collect(child, children, placed);
    }
}

fn node<'a>(
    id: &'a str,
    children: &BTreeMap<&'a str, Vec<&'a str>>,
    lock_of: &HashMap<&str, &LockWait>,
    wait_of: &HashMap<&str, &str>,
    seen: &mut BTreeSet<&'a str>,
    head: bool,
) -> BlockNode {
    seen.insert(id);
    let kids = children.get(id).into_iter().flatten().filter(|c| !seen.contains(**c)).copied().collect::<Vec<_>>();
    let children_nodes = kids.into_iter().map(|c| node(c, children, lock_of, wait_of, seen, false)).collect();
    let lock = if head { None } else { lock_of.get(id).map(|l| (*l).clone()) };
    let waits_for = if head { None } else { lock_phrase(wait_of.get(id).copied(), lock.as_ref()) };
    BlockNode { session: id.to_string(), lock, waits_for, children: children_nodes }
}

fn descendants(node: &BlockNode) -> Vec<&BlockNode> {
    let mut out = Vec::new();
    for child in &node.children {
        out.push(child);
        out.extend(descendants(child));
    }
    out
}

fn most_common<T: Clone + Ord>(items: impl Iterator<Item = T>) -> Option<T> {
    let mut counts: BTreeMap<T, usize> = BTreeMap::new();
    for item in items {
        *counts.entry(item).or_default() += 1;
    }
    counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0))).map(|(k, _)| k)
}

/// The kind of statement a session runs ("SELECT", "UPDATE"): SQL Server's command says it; MySQL's says only "Query",
/// so the statement's first keyword does.
fn statement_verb(session: &Session) -> Option<String> {
    const VERBS: [&str; 12] = [
        "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "CREATE", "ALTER", "DROP", "TRUNCATE", "CALL",
        "WITH",
    ];
    let verb = |word: &str| {
        let upper = word.trim_start_matches('(').to_ascii_uppercase();
        VERBS.contains(&upper.as_str()).then(|| if upper == "WITH" { "SELECT".to_string() } else { upper })
    };
    session
        .command
        .as_deref()
        .and_then(|c| c.split_whitespace().next())
        .and_then(verb)
        .or_else(|| session.sql.as_deref().and_then(|t| t.split_whitespace().next()).and_then(verb))
}

fn head_kind(session: Option<&Session>) -> HeadKind {
    let Some(s) = session else { return HeadKind::Unknown };
    let state = s.state.to_ascii_lowercase();
    if state.contains("idle in transaction") {
        return HeadKind::IdleInTransaction;
    }
    // SQL Server: a sleeping session with an open transaction.
    if (state == "sleeping" || state == "idle") && s.transaction_ms.is_some() {
        return HeadKind::IdleInTransaction;
    }
    if matches!(state.as_str(), "sleeping" | "idle" | "dormant") {
        return HeadKind::Idle;
    }
    HeadKind::Running
}

/// What kind of lock, in words a non-DBA reads: `LCK_M_U` → "an update lock", `X,REC_NOT_GAP` → "an exclusive row lock",
/// a Postgres `ShareLock` on a transaction id → "a row lock".
pub fn lock_kind(wait_type: Option<&str>, lock: Option<&LockWait>) -> Option<String> {
    if let Some(w) = wait_type.filter(|w| w.starts_with("LCK_M_")) {
        let mode = &w["LCK_M_".len()..];
        return Some(
            match mode {
                "S" => "a shared lock",
                "U" => "an update lock",
                "X" => "an exclusive lock",
                "IS" => "an intent-shared lock",
                "IU" => "an intent-update lock",
                "IX" => "an intent-exclusive lock",
                "SIX" | "SIU" | "UIX" => "a shared-and-intent lock",
                "SCH_S" => "a schema-stability lock",
                "SCH_M" => "a schema-change lock",
                "BU" => "a bulk-load lock",
                m if m.starts_with("R") => "a key-range lock",
                _ => "a lock",
            }
            .to_string(),
        );
    }
    let lock = lock?;
    if matches!(lock.resource.as_deref(), Some("transactionid") | Some("tuple")) {
        return Some("a row lock".into());
    }
    let mode = lock.mode.as_deref()?;
    // MySQL performance_schema modes: X, S, IX, IS, plus REC_NOT_GAP / GAP / INSERT_INTENTION.
    let parts: Vec<&str> = mode.split(',').map(str::trim).collect();
    let base = parts.first().copied().unwrap_or("");
    let has = |p: &str| parts.iter().any(|x| x.eq_ignore_ascii_case(p));
    let strength = match base {
        "X" => "an exclusive",
        "S" => "a shared",
        "IX" => "an intent-exclusive",
        "IS" => "an intent-shared",
        "U" => "an update",
        _ => "",
    };
    if !strength.is_empty() {
        let scope = if has("INSERT_INTENTION") {
            " insert-intention"
        } else if has("GAP") {
            " gap"
        } else if has("REC_NOT_GAP") {
            " row"
        } else if lock
            .resource
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case("TABLE") || r.eq_ignore_ascii_case("OBJECT"))
        {
            " table"
        } else {
            ""
        };
        return Some(format!("{strength}{scope} lock"));
    }
    // Postgres table locks: AccessExclusiveLock, ShareRowExclusiveLock, …
    Some(match mode {
        "AccessExclusiveLock" => "a table-wide exclusive lock".to_string(),
        m if m.ends_with("Lock") => format!("a {m}"),
        m => format!("a {m} lock"),
    })
}

/// "an update lock on shop.orders", "a row lock on public.orders", "an exclusive row lock on shop.orders".
pub fn lock_phrase(wait_type: Option<&str>, lock: Option<&LockWait>) -> Option<String> {
    let object = lock.and_then(|l| l.object.as_deref());
    match (lock_kind(wait_type, lock), object) {
        (Some(kind), Some(object)) => Some(format!("{kind} on {object}")),
        (Some(kind), None) => Some(kind),
        (None, Some(object)) => Some(format!("a lock on {object}")),
        (None, None) => wait_type.map(str::to_string),
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn chain(
    engine: Engine,
    head: &str,
    session: Option<&Session>,
    tree: BlockNode,
    by_id: &HashMap<&str, &Session>,
    lock_of: &HashMap<&str, &LockWait>,
) -> BlockingChain {
    let kind = head_kind(session);
    let all = descendants(&tree);
    let waiters = all.len() as u32;
    let longest_wait_ms = all
        .iter()
        .filter_map(|n| by_id.get(n.session.as_str()).and_then(|s| s.wait_ms.or(s.elapsed_ms)))
        .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))));
    let direct: Vec<&str> = tree.children.iter().map(|c| c.session.as_str()).collect();
    // MySQL's "wait" is the processlist state (updating, statistics), not a wait type; the lock says it better.
    let wait_type = if engine == Engine::Mysql {
        None
    } else {
        most_common(direct.iter().filter_map(|id| by_id.get(id).and_then(|s| s.wait.clone())))
    };
    let lock = most_common(
        direct.iter().filter_map(|id| lock_of.get(id)).map(|l| (l.object.clone(), l.mode.clone(), l.resource.clone())),
    )
    .and_then(|(object, mode, resource)| {
        direct
            .iter()
            .filter_map(|id| lock_of.get(id))
            .find(|l| (&l.object, &l.mode, &l.resource) == (&object, &mode, &resource))
            .map(|l| (*l).clone())
    });
    let head_ms = session.and_then(|s| s.elapsed_ms);
    let transaction_ms = session.and_then(|s| s.transaction_ms);
    let on = lock_phrase(wait_type.as_deref(), lock.as_ref()).map(|p| format!(" ({p})")).unwrap_or_default();
    let wait_type = wait_type.filter(|w| w.starts_with("LCK_M_") || w.starts_with("Lock"));
    let behind = format!("{} behind it{on}", plural(waiters, "session waits", "sessions wait"));
    let headline = match kind {
        HeadKind::IdleInTransaction => match head_ms {
            Some(ms) => {
                format!("Session {head} has been idle in an open transaction for {}; {behind}.", format::duration(ms))
            }
            None => format!("Session {head} is idle in an open transaction; {behind}."),
        },
        HeadKind::Running => {
            let what = match session.and_then(statement_verb) {
                Some(v) if v.starts_with(['A', 'E', 'I', 'O', 'U']) => format!("an {v} statement"),
                Some(v) => format!("a {v} statement"),
                None => "a statement".to_string(),
            };
            match head_ms {
                Some(ms) => format!(
                    "Session {head} has been running {what} for {} while holding locks; {behind}.",
                    format::duration(ms)
                ),
                None => format!("Session {head} is running {what} while holding locks; {behind}."),
            }
        }
        HeadKind::Idle => format!("Session {head} holds locks without running anything; {behind}."),
        HeadKind::Unknown => format!("Session {head}, which this login can't see, holds the locks; {behind}."),
    };
    let advice = advice(engine, kind, head, wait_type.as_deref());
    BlockingChain {
        head: head.to_string(),
        head_kind: kind,
        head_ms,
        transaction_ms,
        waiters,
        longest_wait_ms,
        wait_type,
        lock,
        headline,
        advice,
        tree,
    }
}

fn advice(engine: Engine, kind: HeadKind, head: &str, wait_type: Option<&str>) -> String {
    let readers = wait_type.is_some_and(|w| w == "LCK_M_S" || w == "LCK_M_IS");
    match kind {
        HeadKind::IdleInTransaction => {
            let prevent = match engine {
                Engine::Mssql => "SET XACT_ABORT ON in the application makes an error roll the transaction back instead of leaving it open.",
                Engine::Postgres => "Set idle_in_transaction_session_timeout so the server ends sessions that sit in a transaction.",
                Engine::Mysql => "Keep transactions short; waiters give up after innodb_lock_wait_timeout (50 s by default).",
            };
            format!(
                "The application opened a transaction and stopped sending statements: usually a missing COMMIT, an error path \
                 that skips the ROLLBACK, or slow work (an HTTP call, a user prompt) done while the transaction is open. \
                 Ending session {head} rolls its transaction back and frees everyone behind it. {prevent}"
            )
        }
        HeadKind::Running => {
            let readers_note = match (engine, readers) {
                (Engine::Mssql, true) => " The waiters only want to read: with READ_COMMITTED_SNAPSHOT on, readers use row versions and stop waiting on writers.",
                _ => "",
            };
            format!(
                "A long statement is holding locks the others need. Look at its plan in Queries, and keep the transaction \
                 around it short: do reads and outside calls before it starts.{readers_note}"
            )
        }
        HeadKind::Idle => format!(
            "Session {head} isn't running anything but still holds locks, typically from a transaction the client never \
             finished. Ending it releases them."
        ),
        HeadKind::Unknown => {
            "The blocker belongs to another database or a login Culprit can't see; ask someone with VIEW SERVER STATE (or \
             pg_read_all_stats / PROCESS) to look."
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, state: &str, blocked_by: Option<&str>) -> Session {
        Session {
            id: id.into(),
            user: Some("app".into()),
            application: None,
            host: None,
            database: Some("shop".into()),
            state: state.into(),
            command: None,
            wait: blocked_by.map(|_| "LCK_M_U".into()),
            wait_ms: blocked_by.map(|_| 30_000.0),
            blocked_by: blocked_by.map(str::to_string),
            elapsed_ms: Some(48_000.0),
            transaction_ms: Some(49_000.0),
            cpu_ms: None,
            reads: None,
            sql: None,
        }
    }

    fn lock(session: &str) -> LockWait {
        LockWait {
            session: session.into(),
            mode: Some("U".into()),
            resource: Some("KEY".into()),
            object: Some("shop.orders".into()),
            index: Some("PK_orders".into()),
        }
    }

    #[test]
    fn a_running_head_is_named_by_its_statement() {
        // MySQL's process list says "Query" for every statement; the statement's first keyword says what it is.
        let mut head = session("114", "running", None);
        head.command = Some("Query".into());
        head.sql = Some("select sleep(40)".into());
        let waiter = session("115", "suspended", Some("114"));
        let b = analyze(Engine::Mysql, &[head.clone(), waiter.clone()], &[lock("115")]).unwrap();
        assert!(b.chains[0].headline.contains("running a SELECT statement"), "{}", b.chains[0].headline);
        // SQL Server's command is already the verb.
        head.command = Some("UPDATE".into());
        head.sql = None;
        let b = analyze(Engine::Mssql, &[head, waiter], &[lock("115")]).unwrap();
        assert!(b.chains[0].headline.contains("running an UPDATE statement"), "{}", b.chains[0].headline);
    }

    #[test]
    fn nobody_waiting_is_no_blocking() {
        assert!(analyze(Engine::Mssql, &[session("51", "running", None)], &[]).is_none());
    }

    #[test]
    fn an_idle_head_with_a_tree_of_waiters() {
        let sessions = [
            session("57", "sleeping", None),
            session("61", "suspended", Some("57")),
            session("64", "suspended", Some("61")),
            session("70", "suspended", Some("57")),
            session("80", "running", None),
        ];
        let b = analyze(Engine::Mssql, &sessions, &[lock("61"), lock("70")]).unwrap();
        assert_eq!(b.chains.len(), 1);
        let c = &b.chains[0];
        assert_eq!((c.head.as_str(), c.head_kind, c.waiters), ("57", HeadKind::IdleInTransaction, 3));
        assert_eq!(b.waiting, 3);
        assert_eq!(c.tree.children.len(), 2);
        assert_eq!(
            c.headline,
            "Session 57 has been idle in an open transaction for 48.0 s; 3 sessions wait behind it (an update lock on shop.orders)."
        );
        assert!(c.advice.contains("XACT_ABORT"));
    }

    #[test]
    fn a_blocker_this_login_cannot_see() {
        let b = analyze(Engine::Postgres, &[session("61", "active", Some("99"))], &[]).unwrap();
        assert_eq!(b.chains[0].head_kind, HeadKind::Unknown);
        assert!(b.chains[0].headline.contains("can't see"));
    }

    #[test]
    fn a_cycle_still_gets_a_head() {
        let b = analyze(
            Engine::Mssql,
            &[session("61", "suspended", Some("62")), session("62", "suspended", Some("61"))],
            &[],
        )
        .unwrap();
        assert_eq!(b.chains.len(), 1);
        assert_eq!(b.chains[0].waiters, 1);
    }

    #[test]
    fn lock_names_read_as_words() {
        let mysql = LockWait {
            session: "9".into(),
            mode: Some("X,REC_NOT_GAP".into()),
            resource: Some("RECORD".into()),
            object: Some("shop.orders".into()),
            index: Some("PRIMARY".into()),
        };
        assert_eq!(lock_phrase(None, Some(&mysql)).unwrap(), "an exclusive row lock on shop.orders");
        assert_eq!(lock_phrase(Some("LCK_M_SCH_M"), None).unwrap(), "a schema-change lock");
        let gap = LockWait { mode: Some("X,GAP,INSERT_INTENTION".into()), ..mysql.clone() };
        assert_eq!(lock_kind(None, Some(&gap)).unwrap(), "an exclusive insert-intention lock");
        let ddl = LockWait {
            session: "1".into(),
            mode: Some("AccessExclusiveLock".into()),
            resource: Some("relation".into()),
            object: Some("public.orders".into()),
            index: None,
        };
        assert_eq!(lock_phrase(None, Some(&ddl)).unwrap(), "a table-wide exclusive lock on public.orders");
        assert_eq!(lock_phrase(Some("PAGEIOLATCH_SH"), None).unwrap(), "PAGEIOLATCH_SH");
    }

    #[test]
    fn postgres_row_locks_read_as_row_locks() {
        let l = LockWait {
            session: "9".into(),
            mode: Some("ShareLock".into()),
            resource: Some("transactionid".into()),
            object: Some("public.orders".into()),
            index: None,
        };
        assert_eq!(lock_phrase(Some("Lock: transactionid"), Some(&l)).unwrap(), "a row lock on public.orders");
    }
}
