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
    let (sessions, targets, filter, startup) = if azure_sql_database {
        ("sys.dm_xe_database_sessions", "sys.dm_xe_database_session_targets", String::new(), "")
    } else {
        (
            "sys.dm_xe_sessions",
            "sys.dm_xe_session_targets",
            format!("\n    WHERE sqlserver.database_name = N'{}'", database.replace('\'', "''")),
            "\nWITH (STARTUP_STATE = ON)",
        )
    };
    // One session catches the escalations on every table: each table's finding starts the same one, once.
    let start = super::event_session_sql(
        ESCALATION_SESSION,
        azure_sql_database,
        &format!(
            "ADD EVENT sqlserver.lock_escalation (\n    \
                 SET collect_statement = 1\n    \
                 ACTION (sqlserver.sql_text, sqlserver.client_app_name){filter})\n\
             ADD TARGET package0.ring_buffer{startup}"
        ),
    );
    let stop = super::event_session_drop(ESCALATION_SESSION, azure_sql_database);
    format!(
        "-- SQL Server doesn't record which statement escalated: this session keeps the next ones with their statement.\n\
         -- It catches them on every table, so another table's finding starts this same session: once is enough.\n\
         {start}\n\
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
          CROSS APPLY r.d.nodes('RingBufferTarget/event') e(x);\n\
         \n\
         {stop}"
    )
}

/// Why a read of `schema.table` keeps its row locks until the statement ends, which is how a read escalates: under read
/// committed a read gives each row lock up as it moves on, but a plan that prefetches its key lookups on the table
/// (the Nested Loops' `WithUnorderedPrefetch` or `WithOrderedPrefetch`) keeps them, as a locking hint does. Under
/// READ_COMMITTED_SNAPSHOT a read under read committed takes no row locks, so only the hint is left.
fn keeps_row_locks(q: &QueryStat, schema: &str, table: &str, rcsi: bool) -> Option<&'static str> {
    if ["holdlock", "updlock", "xlock", "repeatableread", "serializable"].iter().any(|h| mentions_word(&q.text, h)) {
        return Some("it asks for a locking hint, which can retain row locks until its transaction ends");
    }
    let plan = q.plan.as_ref().filter(|_| !rcsi)?;
    let looks_up = plan.nodes().iter().any(|n| {
        n.kind == culprit_core::NodeKind::Lookup && n.object.as_deref().is_some_and(|o| names_table(o, schema, table))
    });
    let prefetches = plan
        .raw
        .as_deref()
        .is_some_and(|raw| raw.contains("WithUnorderedPrefetch=\"1\"") || raw.contains("WithOrderedPrefetch=\"1\""));
    (looks_up && prefetches)
        .then_some("its plan prefetches its key lookups, which keeps their row locks until the statement ends")
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
    let rcsi = snapshot.setting("is_read_committed_snapshot_on").is_some_and(|s| crate::config::is_on(&s.value));
    // Counting for less than a day: every escalation counted is that recent ("3 minutes", "5 hours").
    let started_ago = counter_days(snapshot).filter(|d| *d < 1.0).map(|d| match d * 24.0 {
        h if h >= 1.5 => format!("{} hours", h.round()),
        h => match (h * 60.0).round().max(1.0) {
            1.0 => "a minute".to_string(),
            m => format!("{m} minutes"),
        },
    });
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
            f.verification_basis = culprit_core::VerificationBasis::CounterChange;
            // How many since the earlier scan, when there is one to compare with and the counts compare.
            let new = before.and_then(|b| b.since(&name, t.escalations).map(|n| (b, n)));
            let historical = new.is_some_and(|(_, n)| n == 0.0);
            if historical { f.actionability = culprit_core::Actionability::History; }
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
                            " That is once a day or more on average over the counter history; it does not date individual events."
                        }
                        Some(r) if r >= ESCALATIONS_MEDIUM_PER_DAY => {
                            " That is once a week or more on average over the counter history; it does not establish current recurrence."
                        }
                        Some(_) => " That is less than once a week on average since then: they may all be old.",
                        None => " SQL Server started too recently to tell how often it happens.",
                    };
                    let since = started_on.as_deref().map(|on| format!("since it started, {on}")).unwrap_or_else(|| since_start.clone());
                    if let Some(ago) = started_ago.as_deref() {
                        format!(
                            "SQL Server replaced the row and page locks on {table} with one lock on the whole table {} {since}, {ago} before this \
                             scan, so none is older than that. The next scan compares with this one and says whether any happened since.{often}",
                            times(t.escalations)
                        )
                    } else {
                        format!(
                            "SQL Server replaced the row and page locks on {table} with one lock on the whole table {} {since}. It keeps no time for each, \
                             and Culprit has no earlier scan to compare with, so they may be old: the next scan compares with this one and says \
                             whether any happened since.{often}",
                            times(t.escalations)
                        )
                    }
                }
            };

            // SQL Server doesn't record which statement escalated: the writes to the table that change the most rows
            // per run are the likely ones. Writes to it, not statements that only read it (an INSERT elsewhere that
            // checks it in an EXISTS takes no locks on it to escalate).
            let mut writes: Vec<&QueryStat> =
                app_queries(snapshot).filter(|q| writes_table(q, Engine::Mssql, &schema, &table)).collect();
            let rows_per_run = |q: &QueryStat| q.rows.map(|r| per_run(r, q.executions));
            // Rows a run as whole rows past a handful and to a tenth below it, so 252 rows in 257 runs reads "1 row"
            // next to another statement's "1 row", not "1.0 rows".
            let rows_words = |rows: f64| {
                let n = if rows >= 10.0 { rows.round() } else { (rows * 10.0).round() / 10.0 };
                format!("{} {}", format::count(n), if n == 1.0 { "row" } else { "rows" })
            };
            writes.sort_by(|a, b| {
                rows_per_run(b).unwrap_or(0.0).total_cmp(&rows_per_run(a).unwrap_or(0.0)).then(b.total_time_ms.total_cmp(&a.total_time_ms))
            });
            writes.truncate(3);
            // With no write to it in the query history, a read can be the one: its reads that return the most rows a
            // run, those known to keep their row locks first.
            let mut reads: Vec<&QueryStat> = if writes.is_empty() && !historical {
                queries_touching(snapshot, &schema, &table).into_iter().filter(|q| !is_monitoring_query(q) && is_read(q)).collect()
            } else {
                Vec::new()
            };
            let keeps = |q: &QueryStat| keeps_row_locks(q, &schema, &table, rcsi);
            reads.sort_by(|a, b| {
                keeps(b).is_some().cmp(&keeps(a).is_some()).then(rows_per_run(b).unwrap_or(0.0).total_cmp(&rows_per_run(a).unwrap_or(0.0)))
            });
            reads.truncate(3);
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
                            " SQL Server doesn't record which statement it was; the likeliest is {}, which changes about {} a run.",
                            query_handle(q),
                            rows_words(rows)
                        ),
                        _ => " SQL Server doesn't record which statement it was; the writes to it are listed below, and the one to look for changes thousands of rows in one run: a purge, a cascade, a backfill.".to_string(),
                    },
                    None if !reads.is_empty() => {
                        let how = if rcsi {
                            "With READ_COMMITTED_SNAPSHOT on, ordinary reads under read committed avoid row locks. Locking hints (HOLDLOCK, UPDLOCK) or stronger isolation can still retain row locks until the transaction ends."
                        } else {
                            "A read can escalate too: an ordinary read under read committed usually releases row locks as it advances, but prefetching key lookups can retain locks until the statement ends. Locking hints (HOLDLOCK, UPDLOCK), REPEATABLE READ and SERIALIZABLE can retain them until the transaction ends. Reading many rows can acquire enough locks for an escalation attempt."
                        };
                        let likeliest = match reads.first().and_then(|q| keeps(q).map(|why| (q, why))) {
                            Some((q, why)) => format!(
                                " The likeliest is {}, which returns about {} a run on average: {why}.",
                                query_handle(q),
                                rows_per_run(q).map(rows_words).unwrap_or_else(|| "an unknown number of rows".into())
                            ),
                            None => " The reads of it are listed below, most rows a run first; the one to look for reads thousands of rows in one run while it keeps its locks.".to_string(),
                        };
                        format!(" SQL Server doesn't record which statement it was, and none of the statements in the query history writes to {table}. {how}{likeliest}")
                    }
                    None => " SQL Server doesn't record which statement it was, and none of the statements in the query history writes to it: look at jobs, migrations and maintenance that change many rows at once.".to_string(),
                }
            };
            f.summary = format!(
                "{happened} SQL Server can attempt escalation around 5,000 locks on one table reference, or under lock-memory pressure. \
                 A successful table lock can block incompatible lock requests until the transaction ends; the escalation counter alone does not establish that other sessions were blocked.{suspects}"
            );
            f.queries = writes
                .iter()
                .map(|q| QueryRef {
                    id: q.id.clone(),
                    plan_node: None,
                    note: rows_per_run(q).map(|rows| format!("Changes about {} a run.", rows_words(rows))),
                })
                .chain(reads.iter().map(|q| QueryRef {
                    id: q.id.clone(),
                    plan_node: None,
                    note: rows_per_run(q).map(|rows| {
                        let why = keeps(q).map(|why| format!(" {}{}.", why[..1].to_uppercase(), &why[1..])).unwrap_or_default();
                        format!("Returns about {} a run.{why}", rows_words(rows))
                    }),
                }))
                .collect();
            f.last_seen = writes.first().or(reads.first()).and_then(|q| q.last_seen.clone());

            let waited = t.lock_waits >= 1.0;
            // One escalation is a lock on the whole table: every other writer waited for it, however briefly. New ones
            // since the previous scan are High; old ones with none since are history: worth preventing, not an
            // emergency. Without an earlier scan the count keeps no time, so only a measured rate raises it.
            f.severity = match new {
                Some(_) if historical => Severity::Low,
                Some(_) => Severity::High,
                None => since_start_severity(first_scan_rate, ESCALATIONS_HIGH_PER_DAY, ESCALATIONS_MEDIUM_PER_DAY),
            };
            let placed_in_window = new.is_some_and(|(b, _)| {
                days_between(&b.taken_at, &snapshot.taken_at).is_some_and(|d| d >= 0.0 && snapshot.options.as_ref().is_none_or(|o| d * 1440.0 <= f64::from(o.window_in_minutes()) + 1.0 / 60.0))
            });
            if !historical && !placed_in_window {
                f.informational();
                f.severity = Severity::Low;
                f.caveats.push("These totals do not establish a new increase or continuing blocking in the selected window. Capture dated events or compare counters wholly within that period before treating this as current pressure.".into());
            }
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
            // The loop is the fix for a statement that changes thousands of rows at once. When every write in the query
            // history changes a few rows a run, the one that escalated isn't among them: finding it comes first, and a
            // DELETE loop over a table written a row at a time would be advice for a statement nobody showed.
            let bulk = writes.first().filter(|q| rows_per_run(q).is_some_and(|r| r >= ESCALATION_THRESHOLD / 5.0));
            let (lead, find) = match (historical, bulk) {
                (true, _) => (
                    format!("If a statement still changes thousands of rows of {table} at once, change it into"),
                    "To learn which statement it is, start the event session below",
                ),
                (false, Some(_)) => (
                    format!("Change the statement that touches thousands of rows of {table} into"),
                    "To be sure it is the one, start the event session below",
                ),
                (false, None) => (
                    format!("None of the statements in the query history changes thousands of rows of {table} at once, so the one that escalated is likely a job, a migration or maintenance. Once the event session below has caught it, change it into"),
                    "Start the event session below",
                ),
            };
            // The loop in the likeliest statement's own verb: a delete or an update; anything else is the reader's to batch.
            let lp = bulk.and_then(|q| match first_verb(main_statement(&q.text)).map(|(_, v)| v) {
                Some("delete") => Some(format!("DELETE TOP (1000) FROM {target} WHERE /* the same filter */;")),
                Some("update") => Some(format!(
                    "UPDATE TOP (1000) {target} SET /* the same changes */ WHERE /* the same filter, leaving out rows already changed */;"
                )),
                _ => None,
            });
            let loop_fix = format!(
                "{lead} smaller batches, for example up to 1,000 changed rows per statement, after checking its access path and lock footprint. \
                 A row limit does not bound all locks acquired or prevent escalation under lock-memory pressure. \
                 Use smaller transactions only where partial commits preserve the application's required atomicity. \
                 An UPDATE loop's filter must leave out rows already changed. A single enclosing transaction can retain locks until commit, so batching is no guarantee against a table lock. \
                 {find} ({ESCALATION_SESSION}): it keeps each escalation with its statement, in memory, at almost no cost."
            );
            // A read keeps its locks for a reason of its own; the loop is for a write the query history doesn't show.
            let read_fix = |q: &QueryStat| {
                let hint = keeps(q).is_some_and(|why| why.contains("hint"));
                format!(
                    "Start the event session below ({ESCALATION_SESSION}) to learn which statement escalates: it keeps each escalation with its statement, in memory, at almost no cost. \
                     If it is a read, it escalates because it keeps its row locks until it ends: {}{}consider reading fewer rows per run after checking the access path; row counts do not bound all locks or prevent escalation under lock-memory pressure. \
                     If it is a statement outside the query history that changes many rows at once (a job, a migration, maintenance), consider smaller batches. Use separate transactions only if partial commits preserve required atomicity; an enclosing transaction may retain locks.",
                    if rcsi {
                        ""
                    } else {
                        "Without locking hints, READ_COMMITTED_SNAPSHOT lets ordinary reads under read committed avoid row locks (it changes what readers see and uses tempdb, so test it first); or "
                    },
                    if hint { "drop the locking hint where the read doesn't need it, or " } else { "" }
                )
            };
            f.fix = Some(Fix {
                summary: match reads.first() {
                    Some(q) => read_fix(q),
                    None => loop_fix,
                },
                sql: Some(match lp {
                    Some(statement) => format!(
                        "-- Example only: check lock footprint and required atomicity before batching\n\
                         WHILE 1 = 1\n\
                         BEGIN\n    \
                             {statement}\n    \
                             IF @@ROWCOUNT = 0 BREAK;\n\
                         END;\n\
                         \n\
                         {session}"
                    ),
                    None => session,
                }),
                cost: None,
            });
            f.verify = vec![format!(
                "Compare new escalations on {table} and observed blocking under comparable workload before and after a justified change. No new events means none were recorded in that interval; an idle interval alone does not verify a fix."
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
