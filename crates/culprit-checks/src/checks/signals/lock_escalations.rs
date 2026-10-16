//! The `lock-escalations` check: Tables where one statement took so many row or page locks that SQL Server replaced them with a lock on the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL never escalate row locks.

use super::*;

/// The metric `lock-escalations` reports the escalations since the previous scan under, when there is one.
pub(crate) const ESCALATIONS_SINCE_METRIC: &str = "Escalations since the previous scan";

/// SQL Server considers escalating once one statement holds about this many locks on one index.
pub(super) const ESCALATION_THRESHOLD: f64 = 5_000.0;

/// Escalations a day since SQL Server started that make a first scan's finding High (one a day: something recurring)
/// or Medium (one a week). Fewer may all be old.
pub(super) const ESCALATIONS_HIGH_PER_DAY: f64 = 1.0;
const ESCALATIONS_MEDIUM_PER_DAY: f64 = 1.0 / 7.0;

pub fn lock_escalations_info() -> CheckInfo {
    CheckInfo {
        id: "lock-escalations".into(),
        title: "Statements that lock a whole table".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Tables where one statement took so many row or page locks that SQL Server replaced them with a lock on the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL never escalate row locks.".into(),
    }
}

check!(LockEscalationsCheck: lock_escalations_info, lock_escalations);

#[derive(Default)]
pub(super) struct TableEscalations {
    escalations: f64,
    lock_waits: f64,
    lock_wait_ms: f64,
}

/// The Extended Events session the lock-escalations fix starts, so the next escalation is kept with its statement.
/// Culprit never starts it; a later Culprit can read it by this name.
pub(crate) const ESCALATION_SESSION: &str = "culprit_lock_escalation";

/// The SQL that starts [`ESCALATION_SESSION`] and reads what it caught: per database on Azure SQL Database, where a
/// session can't see other databases, per server (filtered to `database`) elsewhere.
pub(super) fn escalation_session_sql(database: &str, azure_sql_database: bool) -> String {
    let (scope, sessions, targets, filter, startup) = if azure_sql_database {
        ("DATABASE", "sys.dm_xe_database_sessions", "sys.dm_xe_database_session_targets", String::new(), "")
    } else {
        (
            "SERVER",
            "sys.dm_xe_sessions",
            "sys.dm_xe_session_targets",
            format!("\n    WHERE sqlserver.database_name = N'{}'", database.replace('\'', "''")),
            "\nWITH (STARTUP_STATE = ON)",
        )
    };
    format!(
        "-- SQL Server doesn't record which statement escalated: this session keeps the next ones with their statement\n\
         CREATE EVENT SESSION {ESCALATION_SESSION} ON {scope}\n\
         ADD EVENT sqlserver.lock_escalation (\n    \
             SET collect_statement = 1\n    \
             ACTION (sqlserver.sql_text, sqlserver.client_app_name){filter})\n\
         ADD TARGET package0.ring_buffer{startup};\n\
         ALTER EVENT SESSION {ESCALATION_SESSION} ON {scope} STATE = START;\n\
         \n\
         -- Then read what it caught\n\
         SELECT e.x.value('@timestamp', 'datetime2') AS at_utc,\n       \
                OBJECT_NAME(e.x.value('(data[@name=\"object_id\"]/value)[1]', 'int')) AS table_name,\n       \
                e.x.value('(data[@name=\"escalated_lock_count\"]/value)[1]', 'int') AS locks,\n       \
                e.x.value('(data[@name=\"statement\"]/value)[1]', 'nvarchar(max)') AS statement,\n       \
                e.x.value('(action[@name=\"client_app_name\"]/value)[1]', 'nvarchar(256)') AS app\n  \
           FROM (SELECT CAST(t.target_data AS xml) AS d\n          \
                   FROM {targets} t\n          \
                   JOIN {sessions} s ON s.address = t.event_session_address\n         \
                  WHERE s.name = N'{ESCALATION_SESSION}' AND t.target_name = N'ring_buffer') r\n \
          CROSS APPLY r.d.nodes('RingBufferTarget/event') e(x);"
    )
}

pub fn lock_escalations(snapshot: &Snapshot) -> Vec<Finding> {
    let mut by_table: BTreeMap<(String, String), TableEscalations> = BTreeMap::new();
    for i in &snapshot.indexes {
        let Some(c) = i.contention.as_ref() else { continue };
        // Escalations that happened, never attempts alone: SQL Server's attempt counter moves with its own background
        // work, so on a table of a few dozen rows every index can show an attempt an hour that no statement made.
        if c.lock_escalations <= 0.0 {
            continue;
        }
        let t = by_table.entry((i.schema.clone(), i.table.clone())).or_default();
        t.escalations += c.lock_escalations;
        t.lock_waits += c.lock_waits();
        t.lock_wait_ms += c.lock_wait_ms();
    }
    // SQL Server counts escalations from when it started and keeps no time for each: only an earlier scan tells the
    // recent ones from old ones.
    let before = snapshot.escalations_before.as_ref();
    let started = snapshot.server.started_at.as_deref();
    let azure = snapshot.server.on(culprit_core::Platform::AzureSqlDatabase);
    let mut findings: Vec<Finding> = by_table
        .into_iter()
        .map(|((schema, table), t)| {
            let name = format!("{schema}.{table}");
            let mut f = finding("lock-escalations", &name, Category::Indexes, subject(SubjectKind::Table, &name));
            let times = |n: f64| if n == 1.0 { "once".to_string() } else { format!("{} times", format::count(n)) };
            let since_start = match started {
                Some(at) => format!("since SQL Server started ({})", format::friendly_date(at, &snapshot.taken_at)),
                None => "since its indexes were loaded into memory".to_string(),
            };
            let started_on = started.map(crate::queries::when_phrase);
            // How many since the earlier scan, when there is one to compare with and the counts compare.
            let new = before.and_then(|b| b.since(&name, t.escalations).map(|n| (b, n)));
            let historical = new.is_some_and(|(_, n)| n == 0.0);
            // Without one, how many a day since SQL Server started, over an hour or more of counting.
            let first_scan_rate = if new.is_none() { counter_rate_per_day(snapshot, t.escalations) } else { None };
            let happened = match new {
                Some((b, n)) if n > 0.0 => {
                    let total = if b.restarted {
                        format!(
                            "SQL Server restarted since then{}, and these are all it counted since.",
                            started_on.as_deref().map(|on| format!(" ({on})")).unwrap_or_default()
                        )
                    } else if n < t.escalations {
                        format!("It counted {} in all {since_start}.", format::count(t.escalations))
                    } else {
                        format!("All {} it counted {since_start} came after that scan.", format::count(t.escalations))
                    };
                    f.title = format!(
                        "Row locks on {table} escalated to a table lock {} since {}",
                        times(n),
                        crate::queries::short_when(&b.taken_at)
                    );
                    format!(
                        "SQL Server replaced the row and page locks on {table} with one lock on the whole table {} since the previous scan, {}. {total}",
                        times(n),
                        crate::queries::when_phrase(&b.taken_at)
                    )
                }
                Some((b, _)) => {
                    f.title = format!(
                        "Row locks on {table} escalated to a table lock {} {since_start}, none since {}",
                        times(t.escalations),
                        crate::queries::short_when(&b.taken_at)
                    );
                    format!(
                        "These escalations are history, not something happening now: SQL Server counts them from when it started{}, \
                         and the count on {table} hasn't grown since the previous scan, {}. It doesn't record when each one happened.",
                        started_on.as_deref().map(|on| format!(", {on}")).unwrap_or_default(),
                        crate::queries::when_phrase(&b.taken_at)
                    )
                }
                None => {
                    f.title = format!("Row locks on {table} escalated to a table lock {} {since_start}", times(t.escalations));
                    // Without an earlier scan, only how often they happened on average says whether they recur.
                    let often = match first_scan_rate {
                        Some(r) if r >= ESCALATIONS_HIGH_PER_DAY => {
                            " That is once a day or more on average since then: often enough to be still happening."
                        }
                        Some(r) if r >= ESCALATIONS_MEDIUM_PER_DAY => {
                            " That is once a week or more on average since then: likely to happen again."
                        }
                        Some(_) => " That is less than once a week on average since then: they may all be old.",
                        None => " SQL Server started too recently to tell how often it happens.",
                    };
                    format!(
                        "SQL Server replaced the row and page locks on {table} with one lock on the whole table {} {}. It keeps no time for each, \
                         and Culprit has no earlier scan to compare with, so they may be old: the next scan compares with this one and says \
                         whether any happened since.{often}",
                        times(t.escalations),
                        started_on.as_deref().map(|on| format!("since it started, {on}")).unwrap_or_else(|| since_start.clone())
                    )
                }
            };

            // SQL Server doesn't record which statement escalated: the writes to the table that change the most rows
            // per run are the likely ones.
            let mut writes: Vec<&QueryStat> = queries_touching(snapshot, &schema, &table)
                .into_iter()
                .filter(|q| is_write(q) && !crate::support::is_monitoring_query(q))
                .collect();
            let rows_per_run = |q: &QueryStat| q.rows.map(|r| per_run(r, q.executions));
            writes.sort_by(|a, b| {
                rows_per_run(b).unwrap_or(0.0).total_cmp(&rows_per_run(a).unwrap_or(0.0)).then(b.total_time_ms.total_cmp(&a.total_time_ms))
            });
            writes.truncate(3);
            let suspects = if historical {
                if writes.is_empty() {
                    " None of the statements in the query history writes to it now.".to_string()
                } else {
                    " The writes to it in the query history are listed below for reference; none of them has escalated since the previous scan.".to_string()
                }
            } else {
                match writes.first() {
                    Some(q) => match rows_per_run(q) {
                        Some(rows) if rows >= ESCALATION_THRESHOLD / 5.0 => format!(
                            " SQL Server doesn't record which statement it was; the likeliest is {}, which changes about {} rows a run.",
                            query_handle(q),
                            format::count(rows)
                        ),
                        _ => " SQL Server doesn't record which statement it was; the writes to it are listed below, and the one to look for changes thousands of rows in one run: a purge, a cascade, a backfill.".to_string(),
                    },
                    None => " SQL Server doesn't record which statement it was, and none of the statements in the query history writes to it: look at jobs, migrations and maintenance that change many rows at once.".to_string(),
                }
            };
            f.summary = format!(
                "{happened} An escalation happens when one statement takes about 5,000 locks on one index: \
                 SQL Server swaps them for a single lock on the whole table, and every other session that writes to {table}, or reads it without \
                 row versioning, waits until that statement's transaction ends.{suspects}"
            );
            f.queries = writes
                .iter()
                .map(|q| QueryRef {
                    id: q.id.clone(),
                    plan_node: None,
                    note: rows_per_run(q).map(|rows| format!("Changes about {} rows a run.", format::count(rows))),
                })
                .collect();
            f.last_seen = writes.first().and_then(|q| q.last_seen.clone());

            let waited = t.lock_waits >= 1.0;
            // One escalation is a lock on the whole table: every other writer waited for it, however briefly. New ones
            // since the previous scan are High; old ones with none since are history: worth preventing, not an
            // emergency. Without an earlier scan the count keeps no time, so only a measured rate raises it.
            f.severity = match new {
                Some(_) if historical => Severity::Low,
                Some(_) => Severity::High,
                None => since_start_severity(first_scan_rate, ESCALATIONS_HIGH_PER_DAY, ESCALATIONS_MEDIUM_PER_DAY),
            };
            severity_score(&mut f);
            let locks = |n: f64, new: &str| {
                format!("{} {new}table lock{}", format::count(n), if n == 1.0 { "" } else { "s" })
            };
            f.impact.label = match new {
                Some((_, n)) if n > 0.0 => locks(n, "new "),
                Some(_) => format!("{}, none new", locks(t.escalations, "")),
                None => locks(t.escalations, ""),
            };
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new(ESCALATIONS_METRIC, t.escalations, Unit::Count)];
            if let Some((_, n)) = new {
                f.metrics.push(Metric::new(ESCALATIONS_SINCE_METRIC, n, Unit::Count));
            }
            if waited {
                f.metrics.push(Metric::new("Lock waits on its indexes", t.lock_waits, Unit::Count));
                f.metrics.push(Metric::new("Time waiting", t.lock_wait_ms, Unit::Ms));
            }
            let target = qualified(Engine::Mssql, &schema, &table);
            let session = escalation_session_sql(&snapshot.server.database, azure);
            let lead = if historical {
                format!("None happened since the previous scan. If a statement still changes thousands of rows of {table} at once, change it into")
            } else {
                format!("Change the statement that touches thousands of rows of {table} into")
            };
            f.fix = Some(Fix {
                summary: format!(
                    "{lead} a loop over at most 1,000 rows at a time, each its own statement, so none takes enough locks to escalate. \
                     An UPDATE loop's filter must leave out the rows it already changed, or it never ends. Inside one transaction the loop still holds every row it changed until the commit, \
                     but other sessions then wait only for those rows, not for the whole table. \
                     To learn which statement it is, start the event session below ({ESCALATION_SESSION}): it keeps each escalation with its statement, in memory, at almost no cost."
                ),
                sql: Some(format!(
                    "-- One statement over every matching row escalates; 1,000 at a time doesn't\n\
                     WHILE 1 = 1\n\
                     BEGIN\n    \
                         DELETE TOP (1000) FROM {target} WHERE /* the same filter */;\n    \
                         IF @@ROWCOUNT = 0 BREAK;\n\
                     END;\n\
                     \n\
                     {session}"
                )),
                cost: None,
            });
            f.verify = vec![format!(
                "Scan again later: Culprit compares with this scan, and the finding says how many escalations on {table} happened since. None means it's fixed."
            )];
            f.caveats.push(
                "SQL Server counts escalations since the table's indexes were loaded into memory, usually since it started, and keeps no time for each."
                    .into(),
            );
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score).then(a.id.cmp(&b.id)));
    findings
}
