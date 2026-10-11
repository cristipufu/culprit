//! Deadlocks, explained: the processes and locks in the cycle, which pattern it is, and the fixes that make it go away.
//! Engines parse their own formats (SQL Server deadlock XML, InnoDB status, Postgres log lines) into this model;
//! `classify` names the pattern and writes the plain-language story.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::Engine;

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct DeadlockProcess {
    /// The engine's id for the participant (`process1a2b`, `(1)`, a Postgres pid).
    pub id: String,
    /// The session (SPID, thread id, pid) when known.
    pub session: Option<String>,
    pub victim: bool,
    pub statement: Option<String>,
    /// Stored procedure or function the statement ran in.
    pub procedure: Option<String>,
    pub login: Option<String>,
    pub host: Option<String>,
    pub application: Option<String>,
    pub database: Option<String>,
    pub isolation: Option<String>,
    /// The lock mode it was waiting for.
    pub wait_mode: Option<String>,
    /// Transaction log the victim had written (SQL Server): bigger means more work rolled back.
    pub log_used: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct LockHolder {
    pub process: String,
    pub mode: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct DeadlockResource {
    pub id: String,
    /// `keylock`, `pagelock`, `objectlock`, `ridlock`, `exchangeEvent`, `RECORD`, `TABLE`, `transaction`.
    pub kind: String,
    /// `schema.table`, when known.
    pub object: Option<String>,
    pub index: Option<String>,
    pub holders: Vec<LockHolder>,
    pub waiters: Vec<LockHolder>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum DeadlockPattern {
    /// A read through a nonclustered (secondary) index plus a lookup, against an update of the same rows.
    KeyLookup,
    /// Both read the same rows, then both tried to change them.
    LockConversion,
    /// Each locked one thing, then wanted what the other had: opposite order.
    OppositeOrder,
    /// Row locks escalated to a table lock that collided with another transaction.
    LockEscalation,
    /// The threads of one parallel query waited on each other.
    Parallelism,
    /// A cycle that matches none of the above.
    Cycle,
}

impl DeadlockPattern {
    pub fn label(self) -> &'static str {
        match self {
            DeadlockPattern::KeyLookup => "key-lookup deadlock",
            DeadlockPattern::LockConversion => "read-then-update",
            DeadlockPattern::OppositeOrder => "opposite access order",
            DeadlockPattern::LockEscalation => "lock escalation",
            DeadlockPattern::Parallelism => "intra-query parallelism",
            DeadlockPattern::Cycle => "lock cycle",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct DeadlockFix {
    pub title: String,
    pub detail: String,
    /// Copy-only SQL; Culprit never runs it.
    pub sql: Option<String>,
    /// A finding id that is the same fix, when the scan found it (e.g. `config:read-committed-snapshot`).
    pub finding: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Deadlock {
    /// Stable for the same event: time plus participants.
    pub id: String,
    /// When the engine detected it (RFC 3339 UTC), when known.
    pub at: Option<String>,
    /// Where it came from: `system_health`, `InnoDB status`, `pasted`.
    pub source: String,
    pub processes: Vec<DeadlockProcess>,
    pub resources: Vec<DeadlockResource>,
    pub pattern: DeadlockPattern,
    /// Tables involved, `schema.table`.
    pub objects: Vec<String>,
    pub headline: String,
    pub explanation: String,
    pub fixes: Vec<DeadlockFix>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct DeadlockReport {
    /// Most recent first.
    pub deadlocks: Vec<Deadlock>,
    /// Where they were read from, in words.
    pub source: String,
    pub since_hours: f64,
    /// Deadlocks the server counted, when it keeps a counter but not the graphs (Postgres).
    pub counted: Option<f64>,
    /// Since when `counted` counts (statistics reset), RFC 3339.
    pub counted_since: Option<String>,
    /// Why there are no graphs, or what unlocks more.
    pub note: Option<String>,
}

impl Deadlock {
    /// A deadlock as an engine parsed it, before `classify` names it.
    pub fn new(
        source: &str,
        at: Option<String>,
        processes: Vec<DeadlockProcess>,
        resources: Vec<DeadlockResource>,
    ) -> Deadlock {
        let mut participants: Vec<String> =
            processes.iter().map(|p| p.session.clone().unwrap_or_else(|| p.id.clone())).collect();
        participants.sort();
        let id = format!("{:016x}", fnv(&format!("{}|{}", at.as_deref().unwrap_or(""), participants.join(","))));
        Deadlock {
            id,
            at,
            source: source.to_string(),
            processes,
            resources,
            pattern: DeadlockPattern::Cycle,
            objects: Vec::new(),
            headline: String::new(),
            explanation: String::new(),
            fixes: Vec::new(),
        }
    }

    pub fn victim(&self) -> Option<&DeadlockProcess> {
        self.processes.iter().find(|p| p.victim)
    }

    fn label(&self, process: &str) -> String {
        match self.processes.iter().find(|p| p.id == process).and_then(|p| p.session.clone()) {
            Some(session) => format!("session {session}"),
            None => format!("process {process}"),
        }
    }
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn is_intent(mode: &str) -> bool {
    matches!(mode, "IS" | "IX" | "IU" | "SIX" | "SIU" | "UIX")
}

fn table_level(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    k == "objectlock" || k == "table" || k == "hobtlock"
}

fn row_level(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    k == "keylock" || k == "ridlock" || k == "pagelock" || k == "record" || k == "transaction" || k == "tuple"
}

/// Name the pattern and write the headline, explanation and fixes.
pub fn classify(engine: Engine, deadlock: &mut Deadlock) {
    let d = &*deadlock;
    let objects: Vec<String> =
        d.resources.iter().filter_map(|r| r.object.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let sessions: BTreeSet<&str> = d.processes.iter().filter_map(|p| p.session.as_deref()).collect();

    let parallel = d.resources.iter().any(|r| {
        let k = r.kind.to_ascii_lowercase();
        k == "exchangeevent" || k == "threadpool" || k == "resourcewait"
    }) || (sessions.len() == 1 && d.processes.len() > 1);
    let conversion = d.resources.iter().any(|r| {
        let holders: BTreeSet<&str> = r.holders.iter().map(|h| h.process.as_str()).collect();
        let waiters: BTreeSet<&str> = r.waiters.iter().map(|h| h.process.as_str()).collect();
        holders.len() >= 2 && holders.is_subset(&waiters)
    });
    let escalation = d.resources.iter().any(|r| {
        table_level(&r.kind)
            && r.holders.iter().chain(&r.waiters).any(|h| h.mode.as_deref().is_some_and(|m| !is_intent(m)))
    });
    let rows: Vec<&DeadlockResource> = d.resources.iter().filter(|r| row_level(&r.kind)).collect();
    let indexes_per_object: BTreeMap<&str, BTreeSet<&str>> = rows.iter().fold(BTreeMap::new(), |mut acc, r| {
        if let (Some(o), Some(i)) = (r.object.as_deref(), r.index.as_deref()) {
            acc.entry(o).or_insert_with(BTreeSet::new).insert(i);
        }
        acc
    });
    let key_lookup = indexes_per_object.values().any(|ixs| ixs.len() >= 2);

    let pattern = if parallel {
        DeadlockPattern::Parallelism
    } else if conversion {
        DeadlockPattern::LockConversion
    } else if key_lookup {
        DeadlockPattern::KeyLookup
    } else if escalation {
        DeadlockPattern::LockEscalation
    } else if d.resources.len() >= 2 && d.processes.len() >= 2 {
        DeadlockPattern::OppositeOrder
    } else {
        DeadlockPattern::Cycle
    };

    let victim = d.victim().map(|v| d.label(&v.id));
    let rolled_back = victim.as_ref().map(|v| format!("; {v}'s transaction was rolled back")).unwrap_or_default();
    let tables = match objects.len() {
        0 => "the same data".to_string(),
        1 => objects[0].clone(),
        _ => format!("{} and {}", objects[..objects.len() - 1].join(", "), objects[objects.len() - 1]),
    };
    let (lookup_object, lookup_indexes) = indexes_per_object
        .iter()
        .find(|(_, ixs)| ixs.len() >= 2)
        .map(|(o, ixs)| (o.to_string(), ixs.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
        .unwrap_or_default();
    let secondary = lookup_indexes
        .iter()
        .find(|i| {
            let l = i.to_ascii_lowercase();
            !(l.starts_with("pk") || l == "primary" || l.contains("clust"))
        })
        .cloned();

    let headline = match pattern {
        DeadlockPattern::KeyLookup => format!(
            "Key-lookup deadlock on {lookup_object}: a read through {} and a write to the same rows took their locks in opposite order{rolled_back}.",
            secondary.as_deref().unwrap_or("a secondary index")
        ),
        DeadlockPattern::LockConversion => {
            format!("Two transactions read the same rows of {tables}, then both tried to change them{rolled_back}.")
        }
        DeadlockPattern::OppositeOrder => {
            format!("Two transactions locked {tables} in opposite order and waited on each other{rolled_back}.")
        }
        DeadlockPattern::LockEscalation => {
            format!("A table-wide lock on {tables} (lock escalation) collided with another transaction{rolled_back}.")
        }
        DeadlockPattern::Parallelism => {
            "A parallel query deadlocked with its own threads; SQL Server broke the cycle, but the query lost time."
                .to_string()
        }
        DeadlockPattern::Cycle => format!(
            "{} transactions waited on each other in a cycle on {tables}{rolled_back}.",
            d.processes.len().max(2)
        ),
    };

    let explanation = match pattern {
        DeadlockPattern::KeyLookup => format!(
            "The read finds rows through {} and then looks each one up in the table's main index; the write changes the \
             main index first and then {}. Each ends up holding the lock the other needs next. It recurs whenever the two \
             statements touch the same rows at the same time.",
            secondary.as_deref().unwrap_or("a secondary index"),
            secondary.as_deref().unwrap_or("the secondary index")
        ),
        DeadlockPattern::LockConversion => "Both took a shared lock to read, then asked for an exclusive lock to write. A \
             shared lock lets the other reader in, so neither can upgrade: whoever asks second deadlocks. Typical of \
             'read, check, then update' code."
            .to_string(),
        DeadlockPattern::OppositeOrder => match who_held_what(d).or_else(|| who_ran_what(d)) {
            Some(story) => format!("{story}. With the same order in both, the second would simply wait instead of deadlocking."),
            None => "Each transaction locked one thing and then wanted what the other had already locked. With the same \
                     order in both, the second would simply wait instead of deadlocking."
                .to_string(),
        },
        DeadlockPattern::LockEscalation => "SQL Server turned thousands of row locks into one lock on the whole table to save \
             memory. That table lock blocks everyone touching the table, so an otherwise harmless overlap became a cycle."
            .to_string(),
        DeadlockPattern::Parallelism => "Threads of the same parallel plan exchange rows and waited on each other. These are \
             usually a symptom of a plan that goes parallel when it shouldn't (a scan that an index would avoid)."
            .to_string(),
        DeadlockPattern::Cycle => "Each transaction holds a lock the next one needs. Consistent access order and shorter \
             transactions break most cycles."
            .to_string(),
    };

    let mut fixes = match (pattern, engine) {
        (DeadlockPattern::KeyLookup, Engine::Mssql) => vec![
            DeadlockFix {
                title: "Make the read skip the lookup".into(),
                detail: format!(
                    "Add the columns the read needs to {} (as INCLUDE columns) so it never touches the main index the \
                     write locks. The query's plan in Queries shows the Key Lookup and its output columns.",
                    secondary.as_deref().unwrap_or("the index it seeks")
                ),
                sql: None,
                finding: None,
            },
            rcsi(d),
        ],
        (DeadlockPattern::KeyLookup, _) => vec![DeadlockFix {
            title: "Change rows through the primary key".into(),
            detail: "Look up the primary key values first, then update or lock by primary key, so both statements lock the \
                     main index in the same order."
                .into(),
            sql: None,
            finding: None,
        }],
        (DeadlockPattern::LockConversion, engine) => vec![DeadlockFix {
            title: "Take the write lock when you read".into(),
            detail: "Read the rows you are about to change with an update lock, or do the check and the change in one \
                     UPDATE … WHERE statement."
                .into(),
            sql: Some(match engine {
                Engine::Mssql => "SELECT … FROM … WITH (UPDLOCK, ROWLOCK) WHERE …;".into(),
                _ => "SELECT … FROM … WHERE … FOR UPDATE;".into(),
            }),
            finding: None,
        }],
        (DeadlockPattern::OppositeOrder | DeadlockPattern::Cycle, _) => vec![
            DeadlockFix {
                title: "Touch tables and rows in the same order everywhere".into(),
                detail: format!(
                    "Make every code path that changes {tables} do it in one order (for example, always the parent row \
                     first, and rows sorted by key). Then the second transaction waits instead of deadlocking."
                ),
                sql: None,
                finding: None,
            },
            DeadlockFix {
                title: "Keep transactions short".into(),
                detail: "Do reads, validation and outside calls before the transaction starts; hold locks only for the \
                         writes."
                    .into(),
                sql: None,
                finding: None,
            },
        ],
        (DeadlockPattern::LockEscalation, _) => vec![DeadlockFix {
            title: "Change fewer rows per statement".into(),
            detail: "Modify in batches (for example 2,000 rows at a time) and index the WHERE columns, so SQL Server keeps row \
                     locks instead of locking the whole table."
                .into(),
            sql: None,
            finding: None,
        }],
        (DeadlockPattern::Parallelism, _) => vec![DeadlockFix {
            title: "Fix the plan that goes parallel".into(),
            detail: "Find the query in Queries: usually a scan an index would avoid. As a stopgap, OPTION (MAXDOP 1) on that \
                     query, or raise cost threshold for parallelism if it is still 5."
                .into(),
            sql: None,
            finding: Some("config:cost-threshold-for-parallelism".into()),
        }],
    };
    if pattern != DeadlockPattern::Parallelism {
        fixes.push(DeadlockFix {
            title: "Retry the victim".into(),
            detail: format!(
                "A deadlock victim can be retried safely; the application should catch {} and run the transaction again.",
                match engine {
                    Engine::Mssql => "error 1205",
                    Engine::Postgres => "SQLSTATE 40P01",
                    Engine::Mysql => "error 1213",
                }
            ),
            sql: None,
            finding: None,
        });
    }

    deadlock.pattern = pattern;
    deadlock.objects = objects;
    deadlock.headline = headline;
    deadlock.explanation = explanation;
    deadlock.fixes = fixes;
}

/// "Session 73 held shop.orders and wanted shop.customers; session 75 held shop.customers and wanted shop.orders".
fn who_held_what(d: &Deadlock) -> Option<String> {
    // Without table names (Postgres names row locks by transaction id) the statements tell it better.
    if d.resources.iter().any(|r| r.object.is_none()) {
        return None;
    }
    let thing = |r: &DeadlockResource| -> String {
        match (&r.object, &r.index) {
            (Some(o), Some(i)) if d.resources.iter().filter(|x| x.object.as_ref() == Some(o)).count() > 1 => {
                format!("a row of {o} ({i})")
            }
            (Some(o), _) => o.clone(),
            (None, _) => r.kind.clone(),
        }
    };
    let mut parts = Vec::new();
    for p in &d.processes {
        let held = d.resources.iter().find(|r| r.holders.iter().any(|h| h.process == p.id))?;
        let wanted = d.resources.iter().find(|r| r.waiters.iter().any(|h| h.process == p.id))?;
        let held = thing(held);
        let wanted = thing(wanted);
        if held == wanted {
            return None;
        }
        parts.push(format!("{} held {held} and wanted {wanted}", d.label(&p.id)));
    }
    let mut story = parts.join("; ");
    if let Some(first) = story.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    (!story.is_empty()).then_some(story)
}

/// "Each transaction had already changed rows the other one wanted next: session 539 was running “UPDATE customers …”
/// and session 545 “UPDATE orders …”".
fn who_ran_what(d: &Deadlock) -> Option<String> {
    let clip = |sql: &str| {
        let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() > 60 { format!("{}…", flat.chars().take(59).collect::<String>()) } else { flat }
    };
    let [a, b] = d.processes.as_slice() else { return None };
    let (sa, sb) = (a.statement.as_deref()?, b.statement.as_deref()?);
    Some(format!(
        "Each transaction had already changed rows the other one wanted next: {} was running “{}” and {} “{}”",
        d.label(&a.id),
        clip(sa),
        d.label(&b.id),
        clip(sb)
    ))
}

fn rcsi(d: &Deadlock) -> DeadlockFix {
    let database = d.processes.iter().find_map(|p| p.database.clone());
    DeadlockFix {
        title: "Let reads stop taking shared locks".into(),
        detail:
            "With READ_COMMITTED_SNAPSHOT on, reads use row versions instead of shared locks, so a read can't be part \
                 of this cycle. It costs some tempdb space for the versions."
                .into(),
        sql: database.map(|db| {
            format!(
                "ALTER DATABASE [{}] SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;",
                db.replace(']', "]]")
            )
        }),
        finding: Some("config:read-committed-snapshot".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(id: &str, session: &str, victim: bool) -> DeadlockProcess {
        DeadlockProcess {
            id: id.into(),
            session: Some(session.into()),
            victim,
            database: Some("shop".into()),
            ..Default::default()
        }
    }

    fn resource(kind: &str, object: &str, index: &str, holder: (&str, &str), waiter: (&str, &str)) -> DeadlockResource {
        DeadlockResource {
            id: format!("{kind}-{object}-{index}"),
            kind: kind.into(),
            object: Some(object.into()),
            index: Some(index.into()),
            holders: vec![LockHolder { process: holder.0.into(), mode: Some(holder.1.into()) }],
            waiters: vec![LockHolder { process: waiter.0.into(), mode: Some(waiter.1.into()) }],
        }
    }

    #[test]
    fn a_key_lookup_deadlock_is_named_and_offers_a_covering_index_and_rcsi() {
        let mut d = Deadlock::new(
            "pasted",
            Some("2026-10-11T12:00:00Z".into()),
            vec![process("p1", "61", true), process("p2", "64", false)],
            vec![
                resource("keylock", "shop.customers", "ix_customers_email", ("p1", "S"), ("p2", "X")),
                resource("keylock", "shop.customers", "PK_customers", ("p2", "X"), ("p1", "S")),
            ],
        );
        classify(Engine::Mssql, &mut d);
        assert_eq!(d.pattern, DeadlockPattern::KeyLookup);
        assert!(d.headline.starts_with("Key-lookup deadlock on shop.customers: a read through ix_customers_email"));
        assert!(d.headline.contains("session 61's transaction was rolled back"));
        assert!(d.fixes.iter().any(|f| f.finding.as_deref() == Some("config:read-committed-snapshot")));
        assert!(d.fixes.iter().any(|f| f.sql.as_deref().is_some_and(|s| s.contains("[shop]"))));
    }

    #[test]
    fn opposite_order_conversion_and_parallelism() {
        let mut d = Deadlock::new(
            "pasted",
            None,
            vec![process("p1", "61", true), process("p2", "64", false)],
            vec![
                resource("keylock", "shop.orders", "PK_orders", ("p1", "X"), ("p2", "X")),
                resource("keylock", "shop.customers", "PK_customers", ("p2", "X"), ("p1", "X")),
            ],
        );
        classify(Engine::Mssql, &mut d);
        assert_eq!(d.pattern, DeadlockPattern::OppositeOrder);
        assert!(d.headline.contains("shop.customers and shop.orders in opposite order"));

        let shared = DeadlockResource {
            id: "r".into(),
            kind: "keylock".into(),
            object: Some("shop.orders".into()),
            index: Some("PK_orders".into()),
            holders: vec![
                LockHolder { process: "p1".into(), mode: Some("S".into()) },
                LockHolder { process: "p2".into(), mode: Some("S".into()) },
            ],
            waiters: vec![
                LockHolder { process: "p1".into(), mode: Some("X".into()) },
                LockHolder { process: "p2".into(), mode: Some("X".into()) },
            ],
        };
        let mut c =
            Deadlock::new("pasted", None, vec![process("p1", "61", false), process("p2", "64", true)], vec![shared]);
        classify(Engine::Mysql, &mut c);
        assert_eq!(c.pattern, DeadlockPattern::LockConversion);
        assert!(c.fixes[0].sql.as_deref().unwrap().contains("FOR UPDATE"));

        let mut p = Deadlock::new(
            "pasted",
            None,
            vec![process("p1", "70", false), process("p2", "70", false)],
            vec![DeadlockResource { id: "e".into(), kind: "exchangeEvent".into(), ..Default::default() }],
        );
        classify(Engine::Mssql, &mut p);
        assert_eq!(p.pattern, DeadlockPattern::Parallelism);
    }

    #[test]
    fn the_same_event_keeps_its_id() {
        let a =
            Deadlock::new("x", Some("t".into()), vec![process("p1", "61", true), process("p2", "64", false)], vec![]);
        let b =
            Deadlock::new("y", Some("t".into()), vec![process("p2", "64", false), process("p1", "61", true)], vec![]);
        assert_eq!(a.id, b.id);
    }
}
