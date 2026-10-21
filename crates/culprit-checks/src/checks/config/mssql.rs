//! SQL Server configuration and Query Store health checks.

use super::*;

pub(super) fn check(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = settings(snapshot);
    out.extend(health(snapshot));
    out
}

fn settings(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let db = quote(Engine::Mssql, &snapshot.server.database);

    let qs_actual = value(snapshot, &["query_store_options.actual_state_desc"]);
    let qs_reason =
        value(snapshot, &["query_store_options.readonly_reason"]).and_then(|r| r.trim().parse::<u64>().ok());
    match qs_actual.map(|v| v.to_ascii_uppercase()) {
        Some(state) if state.contains("OFF") => out.push(setting_finding(
            "query-store",
            Severity::Medium,
            "Query Store is off, so query history is lost on every restart".into(),
            "Without Query Store, Culprit (and you) only see what is still in the plan cache: no history, no plan regressions, no per-query waits. It costs little and is on by default in Azure SQL and SQL Server 2022.".into(),
            Fix {
                summary: "Turn it on with automatic capture so ad-hoc noise isn't recorded.".into(),
                sql: Some(format!("ALTER DATABASE {db} SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE, QUERY_CAPTURE_MODE = AUTO);")),
                cost: Some("Uses some space in the database (default cap 1 GB) and a little CPU.".into()),
            },
        )),
        Some(state) if state.contains("READ_ONLY") => {
            if let Some(f) = query_store_read_only(snapshot, &db, qs_reason.unwrap_or(0)) {
                out.push(f);
            }
        }
        _ => {}
    }

    if read_committed_snapshot_off(snapshot) {
        out.push(read_committed_snapshot(snapshot, &db));
    }

    for (name, label) in
        [("is_auto_create_stats_on", "AUTO_CREATE_STATISTICS"), ("is_auto_update_stats_on", "AUTO_UPDATE_STATISTICS")]
    {
        if value(snapshot, &[name]).is_some_and(|v| !is_on(v)) {
            // Some products require statistics they create themselves: SharePoint's content databases run with
            // AUTO_CREATE_STATISTICS off by design.
            let create = label == "AUTO_CREATE_STATISTICS";
            let mut f = setting_finding(
                &label.to_ascii_lowercase(),
                if create { Severity::Medium } else { Severity::High },
                format!("{label} is off"),
                "The optimizer relies on statistics to estimate row counts. With this off, estimates go stale or missing and plans degrade over time.".into(),
                Fix { summary: "Turn it back on.".into(), sql: Some(format!("ALTER DATABASE {db} SET {label} ON;")), cost: None },
            );
            if create {
                f.caveats.push("Some products require it off and maintain statistics themselves (SharePoint content databases do): check the vendor's guidance before turning it on.".into());
            }
            out.push(f);
        }
    }

    if let Some(level) = value(snapshot, &["compatibility_level"]).and_then(|v| v.parse::<u32>().ok()) {
        let latest = latest_compat(&snapshot.server);
        if level + 20 <= latest {
            let mut f = setting_finding(
                "compatibility-level",
                Severity::Low,
                format!("The database runs at compatibility level {level}"),
                format!(
                    "{} supports level {latest}, but the optimizer behaves as it did at {level}. Newer levels bring adaptive joins, memory grant feedback and parameter-sensitive plans.",
                    snapshot.server.cloud.as_deref().filter(|_| snapshot.server.platform().is_some_and(|p| p.is_azure())).unwrap_or("The server")
                ),
                Fix {
                    summary: "Raise it after testing; Query Store makes this safe because you can force the old plans for any query that regresses.".into(),
                    sql: Some(format!("ALTER DATABASE {db} SET COMPATIBILITY_LEVEL = {latest};")),
                    cost: None,
                },
            );
            f.confidence = Confidence::Low;
            f.metrics.push(Metric::text("Current level", level.to_string()));
            out.push(f);
        }
    }

    // Read where the server's options can be read (not Azure SQL Database). The threshold decides which plans go
    // parallel, so where nothing can (MAXDOP 1, one processor) it changes nothing.
    let ctfp = value(snapshot, &["cost threshold for parallelism"]).and_then(|v| v.parse::<f64>().ok());
    if ctfp.is_some_and(|v| v <= 5.0) && !serial_only(snapshot) {
        let change = server_option(&snapshot.server, "cost threshold for parallelism", "50");
        out.push(setting_finding(
            "cost-threshold-for-parallelism",
            Severity::Low,
            "Cost threshold for parallelism is still the 1990s default of 5".into(),
            "Even cheap queries go parallel, which wastes CPU on coordination and shows up as CXPACKET/CXCONSUMER waits.".into(),
            Fix {
                summary: format!("A common starting point is 50; adjust with your workload. {}", change.how),
                sql: change.sql,
                cost: None,
            },
        ));
    }
    out
}

fn health(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let db = quote(Engine::Mssql, &snapshot.server.database);
    let qs_on = value(snapshot, &["query_store_options.actual_state_desc"])
        .is_some_and(|v| matches!(v.to_ascii_uppercase().as_str(), "READ_WRITE" | "READ_ONLY"));

    // With size-based cleanup on AUTO, Query Store trims its oldest data at 90% full (down to about 80%) and never
    // fills: nearly full is how it runs, not a warning.
    let cleans_itself = value(snapshot, &["query_store_options.size_based_cleanup_mode_desc"])
        .is_some_and(|v| v.eq_ignore_ascii_case("AUTO"));
    if qs_on
        && !cleans_itself
        && let (Some(used), Some(max)) = (
            number(snapshot, "query_store_options.current_storage_size_mb"),
            number(snapshot, "query_store_options.max_storage_size_mb"),
        )
        && max > 0.0
        && used / max >= 0.9
        && value(snapshot, &["query_store_options.actual_state_desc"])
            .is_some_and(|v| v.eq_ignore_ascii_case("READ_WRITE"))
    {
        let mut f = setting_finding(
            "query-store-size",
            Severity::Medium,
            format!("Query Store is {} full and stops recording when it fills", format::percent(used / max)),
            format!(
                "It holds {} MB of its {} MB limit. At the limit it turns read-only: new queries, plan changes and \
                 regressions stop being captured until someone makes room.",
                used.round(),
                max.round()
            ),
            Fix {
                summary: "Raise the limit and let it clean up old data on its own.".into(),
                sql: Some(format!(
                    "ALTER DATABASE {db} SET QUERY_STORE (MAX_STORAGE_SIZE_MB = {}, SIZE_BASED_CLEANUP_MODE = AUTO);",
                    ((max * 2.0) / 1024.0).ceil() * 1024.0
                )),
                cost: Some("Space in the database itself.".into()),
            },
        );
        f.confidence = Confidence::High;
        f.metrics.push(Metric::new("Used", used / max, Unit::Ratio));
        out.push(f);
    }

    if qs_on
        && value(snapshot, &["query_store_options.query_capture_mode_desc"])
            .is_some_and(|v| v.eq_ignore_ascii_case("ALL"))
    {
        let busy = snapshot.workload.as_ref().is_some_and(|w| w.query_count >= 5000.0);
        // The statements the scan read are those of its window, not everything the store holds: said as what ran in
        // it. A handful crowds nothing, so the cost is said as what ALL would do once one-off statements arrive.
        let effect = match snapshot.workload.as_ref().filter(|w| w.query_count > 0.0) {
            Some(w) if busy => format!(
                " ({} statements in {}). They crowd the store, shorten how far back it can look and add overhead; \
                 AUTO keeps what matters.",
                format::count(w.query_count),
                w.window_phrase()
            ),
            Some(w) => format!(
                ". With {} {} in {} it costs little now; once one-off statements arrive (ad-hoc queries, generated \
                 SQL), they crowd the store, shorten how far back it can look and add overhead. AUTO keeps what \
                 matters.",
                format::count(w.query_count),
                if w.query_count == 1.0 { "statement" } else { "statements" },
                w.window_phrase()
            ),
            None => {
                ". On a busy database they crowd the store, shorten how far back it can look and add overhead; AUTO \
                     keeps what matters."
                    .into()
            }
        };
        let mut f = setting_finding(
            "query-store-capture",
            if busy { Severity::Medium } else { Severity::Low },
            "Query Store records every statement, including one-off ones".into(),
            format!("Capture mode ALL keeps statements that ran once or cost nothing{effect}"),
            Fix {
                summary: "Switch to automatic capture.".into(),
                sql: Some(format!("ALTER DATABASE {db} SET QUERY_STORE (QUERY_CAPTURE_MODE = AUTO);")),
                cost: None,
            },
        );
        f.confidence = Confidence::Medium;
        out.push(f);
    }

    if qs_on
        && value(snapshot, &["query_store_options.wait_stats_capture_mode_desc"])
            .is_some_and(|v| v.eq_ignore_ascii_case("OFF"))
    {
        out.push(setting_finding(
            "query-store-waits",
            Severity::Low,
            "Query Store isn't recording waits, so waits can't be tied to queries".into(),
            "With wait capture off, Culprit can say the server waits on locks or disk but not which queries do the \
             waiting."
                .into(),
            Fix {
                summary: "Turn wait capture on; the overhead is small.".into(),
                sql: Some(format!("ALTER DATABASE {db} SET QUERY_STORE (WAIT_STATS_CAPTURE_MODE = ON);")),
                cost: None,
            },
        ));
    }

    if value(snapshot, &["LEGACY_CARDINALITY_ESTIMATION"]).is_some_and(is_on) {
        let mut f = setting_finding(
            "legacy-cardinality-estimation",
            Severity::Low,
            "The database estimates row counts the pre-2014 way".into(),
            "LEGACY_CARDINALITY_ESTIMATION is on, so the optimizer uses the old estimator regardless of the \
             compatibility level. It was often set as a quick fix after an upgrade and then forgotten."
                .into(),
            Fix {
                summary: "Turn it off after testing, and use Query Store to force the old plan for any query that \
                    regresses (or a per-query hint)."
                    .into(),
                sql: Some("ALTER DATABASE SCOPED CONFIGURATION SET LEGACY_CARDINALITY_ESTIMATION = OFF;".into()),
                cost: None,
            },
        );
        f.confidence = Confidence::Low;
        out.push(f);
    }

    // MAXDOP 0 lets a query use every processor. Up to 8 per NUMA node that is what Microsoft recommends anyway, so it
    // matters only on a bigger server, which the scan has to know.
    let server_maxdop = number(snapshot, "max degree of parallelism");
    let db_maxdop = number(snapshot, "MAXDOP");
    let cpus = snapshot.pressure.as_ref().and_then(|p| p.cpus);
    let nodes = snapshot.pressure.as_ref().and_then(|p| p.numa_nodes).unwrap_or(1.0).max(1.0);
    let per_node = cpus.map(|c| (c / nodes).floor());
    if server_maxdop == Some(0.0)
        && db_maxdop.is_none_or(|v| v == 0.0)
        && let Some(cpus) = cpus.filter(|c| *c > 8.0)
    {
        let cap = per_node.unwrap_or(8.0).clamp(1.0, 8.0);
        let mut f = setting_finding(
            "maxdop",
            Severity::Low,
            format!("One query may use all {} processors (MAXDOP 0)", format::count(cpus)),
            format!(
                "With no limit on parallelism, a single large query can occupy all {} schedulers and make everything \
                 else queue behind it. Microsoft's guidance is at most 8, and no more than the processors in one NUMA \
                 node{}.",
                format::count(cpus),
                if nodes > 1.0 {
                    format!(" ({} here)", format::count(per_node.unwrap_or(cpus)))
                } else {
                    String::new()
                }
            ),
            // Database-scoped configuration came with SQL Server 2016 (Azure SQL has it whatever version it reports);
            // before that the cap is the server's.
            if snapshot.server.platform().is_some_and(|p| p.is_azure_sql())
                || culprit_core::version_at_least(&snapshot.server.version, [13, 0, 0])
            {
                Fix {
                    summary: "Cap it for this database (or for the whole server).".into(),
                    sql: Some(format!("ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = {cap:.0};")),
                    cost: None,
                }
            } else {
                let change = setting_change(&snapshot.server, "max degree of parallelism", &format!("{cap:.0}"));
                Fix {
                    summary: format!(
                        "Cap it for the server: this version has no per-database setting (SQL Server 2016 added it). {}",
                        change.how
                    ),
                    sql: change.sql,
                    cost: None,
                }
            },
        );
        f.confidence = Confidence::Low;
        f.metrics.push(Metric::new("Processors", cpus, Unit::Count));
        out.push(f);
    }
    out
}

/// Whether nothing in this database can run in parallel: its own MAXDOP is 1, or it defers (0) to a server MAXDOP of
/// 1, or the server has a single processor.
fn serial_only(snapshot: &Snapshot) -> bool {
    let db_maxdop = number(snapshot, "MAXDOP");
    let server_maxdop = number(snapshot, "max degree of parallelism");
    let cpus = snapshot.pressure.as_ref().and_then(|p| p.cpus);
    db_maxdop == Some(1.0) || (db_maxdop.is_none_or(|d| d == 0.0) && server_maxdop == Some(1.0)) || cpus == Some(1.0)
}

/// Query Store's `readonly_reason` codes (they add up when several apply).
const READ_ONLY_REASONS: [(u64, &str); 8] = [
    (1, "the database is read-only"),
    (2, "the database is in single-user mode"),
    (4, "the database is in emergency mode"),
    (8, "this is a secondary replica"),
    (65_536, "it reached its size limit (MAX_STORAGE_SIZE_MB)"),
    (131_072, "it holds as many different statements as its memory allows"),
    (262_144, "data waiting to be written to disk reached its memory limit"),
    (524_288, "the database is out of disk space"),
];

/// A read-only Query Store, by why. A read-only database, single-user or emergency mode, or a secondary replica keep
/// it read-only by design: nothing to fix in Query Store. The rest each have their own fix.
fn query_store_read_only(snapshot: &Snapshot, db: &str, reason: u64) -> Option<Finding> {
    const BY_DESIGN: u64 = 1 | 2 | 4 | 8;
    if reason == 0 || reason & BY_DESIGN != 0 {
        return None;
    }
    let reasons: Vec<String> =
        READ_ONLY_REASONS.iter().filter(|(code, _)| reason & code != 0).map(|(_, why)| why.to_string()).collect();
    let why = if reasons.is_empty() { format!("reason code {reason}") } else { format::list(&reasons) };
    let max = value(snapshot, &["query_store_options.max_storage_size_mb"]).and_then(|v| v.trim().parse::<f64>().ok());
    let (summary, sql, severity) = if reason & 65_536 != 0 {
        // Never less room than it has now.
        let room = max.map_or(2048.0, |m| (m * 2.0).max(2048.0));
        (
            "Give it more room and let it clean up old data by itself, then turn recording back on.",
            format!(
                "ALTER DATABASE {db} SET QUERY_STORE (MAX_STORAGE_SIZE_MB = {room:.0}, SIZE_BASED_CLEANUP_MODE = AUTO, OPERATION_MODE = READ_WRITE);"
            ),
            Severity::Medium,
        )
    } else if reason & 131_072 != 0 {
        (
            "Record only statements that matter (automatic capture), then turn recording back on.",
            format!("ALTER DATABASE {db} SET QUERY_STORE (QUERY_CAPTURE_MODE = AUTO, OPERATION_MODE = READ_WRITE);"),
            Severity::Medium,
        )
    } else if reason & 524_288 != 0 {
        (
            "Free or add space in the database's data files, then turn recording back on.",
            format!("ALTER DATABASE {db} SET QUERY_STORE (OPERATION_MODE = READ_WRITE);"),
            Severity::Medium,
        )
    } else {
        (
            "It usually returns to recording once the data is written; if it stays read-only, turn recording back on.",
            format!("ALTER DATABASE {db} SET QUERY_STORE (OPERATION_MODE = READ_WRITE);"),
            Severity::Low,
        )
    };
    Some(setting_finding(
        "query-store",
        severity,
        "Query Store stopped recording".into(),
        format!("Query Store is read-only because {why}. New queries and plan changes aren't being captured."),
        Fix { summary: summary.into(), sql: Some(sql), cost: None },
    ))
}

/// Whether READ_COMMITTED_SNAPSHOT is known to be off for this database (SQL Server): reads under read committed then
/// wait for the locks writes hold on their rows.
pub(crate) fn read_committed_snapshot_off(snapshot: &Snapshot) -> bool {
    snapshot.server.engine == Engine::Mssql
        && value(snapshot, &["is_read_committed_snapshot_on"]).is_some_and(|v| !is_on(v))
}

/// A read whose lock waits row versioning for read committed would remove: under READ_COMMITTED_SNAPSHOT it reads the
/// last committed version of the rows instead of waiting for a write's locks. Not a read that locks what it reads
/// (UPDLOCK, HOLDLOCK): it waits with row versioning on too.
pub(crate) fn versioning_spares_read(q: &culprit_core::QueryStat) -> bool {
    statement_handle(&q.text).starts_with("a read")
        && crate::queries::kind_of(q) != crate::queries::StatementKind::Locking
}

/// Time reads spent waiting on locks in the workload's window, how many deadlocks in this database had a read in them,
/// and the read that waited longest (with its wait): what row versioning for read committed would remove.
fn readers_blocked(snapshot: &Snapshot) -> (f64, usize, Option<(String, f64)>) {
    let waited: Vec<(String, f64)> = app_queries(snapshot)
        .filter(|q| versioning_spares_read(q))
        .map(|q| {
            let ms = q.waits.iter().filter(|w| category_key(&w.category) == "lock").map(|w| w.wait_ms).sum::<f64>();
            (query_handle(q), ms)
        })
        .filter(|(_, ms)| *ms > 0.0)
        .collect();
    let lock_ms: f64 = waited.iter().map(|(_, ms)| ms).sum();
    let longest = waited.into_iter().max_by(|a, b| a.1.total_cmp(&b.1));
    let deadlocks = snapshot
        .deadlocks
        .as_ref()
        .map(|r| {
            r.deadlocks
                .iter()
                .filter(|d| d.fixes.iter().any(|x| x.finding.as_deref() == Some("config:read-committed-snapshot")))
                .count()
        })
        .unwrap_or(0);
    (lock_ms, deadlocks, longest)
}

/// The reads that waited on locks (see [`readers_blocked`]), as their runs came: whether any ran steadily enough for a
/// rate a day, when they ran where they came in bursts ("on Oct 11, 13:56–14:19 UTC", for one read or several in one
/// stretch), and whether all of them stopped running (history: their waits are behind them).
struct WaitingReads<'a> {
    reads: Vec<&'a culprit_core::QueryStat>,
    steady: bool,
    when: Option<String>,
    stopped: bool,
}

fn waiting_reads(snapshot: &Snapshot) -> WaitingReads<'_> {
    let reads: Vec<&culprit_core::QueryStat> = app_queries(snapshot)
        .filter(|q| versioning_spares_read(q))
        .filter(|q| q.waits.iter().any(|w| category_key(&w.category) == "lock" && w.wait_ms > 0.0))
        .collect();
    let w = snapshot.workload.as_ref();
    let steady = reads.iter().any(|q| w.is_some_and(|w| w.runs_per_day_of(q).is_some()));
    // One stretch for all of them: the earliest first run to the latest last, where every read came in one burst.
    let bursts: Vec<(String, String)> = reads
        .iter()
        .filter_map(|q| match w?.cadence(q) {
            culprit_core::Cadence::Burst { from, to, days, .. } if days.is_empty() => Some((from, to)),
            _ => None,
        })
        .collect();
    let when = (!reads.is_empty() && !steady && bursts.len() == reads.len())
        .then(|| {
            let at = |s: &String| crate::support::parse_epoch_seconds(s);
            let from =
                bursts.iter().min_by(|a, b| at(&a.0).partial_cmp(&at(&b.0)).unwrap_or(std::cmp::Ordering::Equal))?;
            let to =
                bursts.iter().max_by(|a, b| at(&a.1).partial_cmp(&at(&b.1)).unwrap_or(std::cmp::Ordering::Equal))?;
            let span = at(&to.1)? - at(&from.0)?;
            (span < 86_400.0).then(|| culprit_core::span_when(&from.0, &to.1))
        })
        .flatten()
        .or_else(|| match reads.as_slice() {
            [q] if !steady => w.and_then(|w| w.cadence(q).when()),
            _ => None,
        });
    let stopped = !reads.is_empty() && reads.iter().all(|q| crate::query_is_history(snapshot, q));
    WaitingReads { reads, steady, when, stopped }
}

fn read_committed_snapshot(snapshot: &Snapshot, db: &str) -> Finding {
    let (lock_ms, deadlocks, longest) = readers_blocked(snapshot);
    let share = snapshot.workload.as_ref().map(|w| w.share(lock_ms)).unwrap_or(0.0);
    let waiting = waiting_reads(snapshot);
    // Reads blocked for a real amount of time (a minute in the window, a few percent of it, and the amount rule every
    // time-based finding uses: about a minute a day, a burst's on its own total), or a deadlock a read was part of:
    // that's what the setting removes. Waits of reads that all stopped running are history, as any finding on them is.
    // Without either it is advice for later.
    let real_amount = severity_for_time_of(snapshot, &waiting.reads, lock_ms, share) >= Severity::Medium;
    let blocking = (lock_ms >= 60_000.0 && share >= 0.03 && real_amount && !waiting.stopped) || deadlocks > 0;
    // When the waits came, where the reads came in bursts, and that they're over where the reads stopped.
    let reads_now = if waiting.reads.len() == 1 { "that read hasn't" } else { "those reads haven't" };
    let came = match (&waiting.when, waiting.stopped) {
        (Some(when), true) => format!(", all of it {when}, and {reads_now} run since"),
        (Some(when), false) => format!(", all of it {when}"),
        (None, true) => format!(", and {reads_now} run since"),
        (None, false) => String::new(),
    };
    let window = snapshot.workload.as_ref().map(|w| w.window_phrase()).unwrap_or_else(|| "this scan".into());
    let (severity, title, evidence) = if blocking {
        let mut parts = Vec::new();
        if lock_ms > 0.0 {
            parts.push(format!(
                "In {window}, reads waited {} on locks held by writes ({} of database time){came}.",
                format::duration(lock_ms),
                format::percent(share)
            ));
        }
        if deadlocks > 0 {
            parts.push(format!(
                "{} deadlock{} in this database had a read in {}.",
                deadlocks,
                if deadlocks == 1 { "" } else { "s" },
                if deadlocks == 1 { "it" } else { "them" }
            ));
        }
        (
            Severity::Medium,
            "Readers and writers block each other (READ_COMMITTED_SNAPSHOT is off)".to_string(),
            format!(" {}", parts.join(" ")),
        )
    } else {
        // Said by its share of database time as well as its amount: a minute that is a fifth of a quiet database's
        // time is little in time, not little of what the database does.
        // Statements' waits come from Query Store's wait capture: a history with none at all recorded can't say reads
        // didn't wait.
        let waits_recorded = app_queries(snapshot).any(|q| !q.waits.is_empty());
        let cost = if lock_ms <= 0.0 && !waits_recorded {
            " Whether reads wait on locks here isn't known: the query history records no waits.".to_string()
        } else if lock_ms <= 0.0 {
            format!(" In {window} no read waited on locks, so it costs nothing now.")
        } else if share < 0.03 {
            format!(
                " In {window} reads waited {} on locks ({} of database time){came}, so it costs little now.",
                format::duration(lock_ms),
                format::percent(share)
            )
        } else {
            // A rate a day only where the reads ran that way; reads that came in bursts are said by when they did.
            let amount = snapshot
                .workload
                .as_ref()
                .filter(|_| waiting.steady)
                .and_then(|w| w.per_day_measured(lock_ms))
                .map(|d| format!("only about {} a day", format::duration(d)))
                .or_else(|| (!came.is_empty()).then(|| came.trim_start_matches(", ").to_string()))
                .unwrap_or_else(|| "little time in all".into());
            let which = match &longest {
                Some((handle, ms)) if *ms >= lock_ms * 0.9 => format!(" (nearly all of it {handle})"),
                Some((handle, ms)) if *ms >= lock_ms * 0.5 => {
                    format!(" (most of it {handle}, {})", format::duration(*ms))
                }
                _ => String::new(),
            };
            format!(
                " In {window} reads waited {} on locks held by writes{which}: {} of database time, but {amount}. With \
                 row versioning on, those reads would read the last committed version of the rows instead of waiting.",
                format::duration(lock_ms),
                format::percent(share)
            )
        };
        (
            Severity::Low,
            "READ_COMMITTED_SNAPSHOT is off: reads would wait for writes on the same rows".to_string(),
            format!("{cost} It matters more as reads and writes meet on the same rows more often."),
        )
    };
    let snapshot_isolation =
        value(snapshot, &["snapshot_isolation_state_desc"]).is_some_and(|v| v.eq_ignore_ascii_case("ON"));
    let mut f = setting_finding(
        "read-committed-snapshot",
        severity,
        title,
        format!(
            "With row versioning off for read committed, a SELECT waits for any write holding a lock on the rows it reads, and vice versa.{evidence} Most OLTP applications run with it on; Azure SQL Database enables it by default."
        ),
        Fix {
            summary: "Turn on row versioning for read committed. It needs a moment with no other connections, so schedule it.".into(),
            sql: Some(format!("ALTER DATABASE {db} SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;")),
            cost: Some(if snapshot_isolation {
                "ALLOW_SNAPSHOT_ISOLATION is already on, so SQL Server already keeps row versions for this database: little extra overhead.".into()
            } else {
                "Row versions live in tempdb (or the database with ADR), which adds some write overhead.".into()
            }),
        },
    );
    if blocking && lock_ms > 0.0 {
        f.metrics.push(Metric::new("Reads waiting on locks", lock_ms, Unit::Ms));
    }
    f.caveats.push("Readers then see the last committed version instead of waiting. Code that relies on blocking reads for correctness (rare) must use locking hints.".into());
    if snapshot_isolation {
        f.caveats.push("ALLOW_SNAPSHOT_ISOLATION is on: code can already opt into SNAPSHOT isolation per transaction; RCSI makes every read-committed read use versions.".into());
    }
    f
}

/// The newest compatibility level the server offers. Azure SQL Database and Managed Instance report a product version
/// of 12.0 whatever they run, so their level comes from the service, not the version: 160 on both today.
fn latest_compat(server: &ServerInfo) -> u32 {
    if server.platform().is_some_and(|p| p.is_azure_sql()) {
        return 160;
    }
    let major: u32 = server.version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(16);
    (major * 10).clamp(100, 170)
}
