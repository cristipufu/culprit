//! Settings that hide the data Culprit (and you) need, or leave obvious performance on the table, per engine: Query
//! Store health on SQL Server, pg_stat_statements and memory/autovacuum defaults on Postgres, performance_schema on
//! MySQL. Part of the `config` check.

use culprit_core::{Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, Unit, format};

use crate::config::{after_restart, is_on, setting_finding, settings_change, value};
use crate::support::{days_between, quote, setting_change};

fn number(snapshot: &Snapshot, name: &str) -> Option<f64> {
    value(snapshot, &[name]).and_then(|v| v.trim().parse::<f64>().ok())
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

/// "128MB", "4GB", "8192kB" (Postgres' pretty values) to bytes.
fn bytes(text: &str) -> Option<f64> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: f64 = n.parse().ok()?;
    let scale = match unit.trim() {
        "B" | "" => 1.0,
        "kB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(n * scale)
}

fn data_bytes(snapshot: &Snapshot) -> f64 {
    snapshot.tables.iter().filter_map(|t| t.total_bytes).sum()
}

pub(crate) fn mssql(snapshot: &Snapshot) -> Vec<Finding> {
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

pub(crate) fn postgres(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    if value(snapshot, &["autovacuum"]).is_some_and(|v| !is_on(v)) {
        let change = setting_change(&snapshot.server, "autovacuum", "on");
        let mut f = setting_finding(
            "autovacuum",
            Severity::High,
            "Autovacuum is off".into(),
            "Dead rows from updates and deletes are never cleaned up automatically, tables and indexes keep growing, \
             planner statistics go stale, and transaction-ID wraparound becomes a real risk."
                .into(),
            Fix {
                summary: format!("Turn it back on; tune it per table instead of disabling it. {}", change.how),
                sql: change.sql,
                cost: None,
            },
        );
        f.confidence = Confidence::High;
        out.push(f);
    }

    // Big tables that churn and still follow the server's scale factor. A table that only grows (no updates or
    // deletes) leaves no dead rows for the factor to wait for, and one with its own factor isn't following it.
    let factor = number(snapshot, "autovacuum_vacuum_scale_factor");
    let big: Vec<&culprit_core::TableStat> = snapshot
        .tables
        .iter()
        .filter(|t| t.rows.unwrap_or(0.0) >= 10_000_000.0)
        .filter(|t| {
            t.storage
                .as_ref()
                .is_none_or(|s| s.option("autovacuum_vacuum_scale_factor").is_none() && !s.autovacuum_off())
        })
        .filter(|t| {
            let churn = t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0);
            t.updates.is_none() && t.deletes.is_none() || churn >= t.rows.unwrap_or(0.0) * 0.01
        })
        .collect();
    if let Some(factor) = factor.filter(|v| *v >= 0.2)
        && !big.is_empty()
    {
        let biggest =
            big.iter().max_by(|a, b| a.rows.unwrap_or(0.0).total_cmp(&b.rows.unwrap_or(0.0))).expect("non-empty");
        let mut f = setting_finding(
            "autovacuum-scale",
            Severity::Low,
            format!("Autovacuum waits for {} of {} to change before cleaning it", format::percent(factor), biggest.name),
            format!(
                "autovacuum_vacuum_scale_factor is {}: {} ({} rows, with updates and deletes) is only vacuumed after \
                 about {} rows change, so dead rows pile up between runs and each run is long.",
                value(snapshot, &["autovacuum_vacuum_scale_factor"]).unwrap_or("0.2"),
                biggest.qualified(),
                format::count(biggest.rows.unwrap_or(0.0)),
                format::count(biggest.rows.unwrap_or(0.0) * factor)
            ),
            Fix {
                summary: "Lower it for the big tables only.".into(),
                sql: Some(
                    big.iter()
                        .take(5)
                        .map(|t| {
                            format!(
                                "ALTER TABLE {} SET (autovacuum_vacuum_scale_factor = 0.01, autovacuum_analyze_scale_factor = 0.02);",
                                crate::support::qualified(Engine::Postgres, &t.schema, &t.name)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                cost: None,
            },
        );
        f.confidence = Confidence::Medium;
        // The SQL names the five biggest: more tables may follow the server's factor after it.
        f.verify = vec![if big.len() > 5 {
            "Re-scan: the tables the SQL changed have their own scale factor and drop out of this finding; the next \
             ones it names need the same change."
                .into()
        } else {
            "Re-scan: the tables have their own scale factor and this finding is gone.".into()
        }];
        out.push(f);
    }

    if value(snapshot, &["pg_stat_statements.track"]).is_some_and(|v| v.eq_ignore_ascii_case("none")) {
        let change = setting_change(&snapshot.server, "pg_stat_statements.track", "top");
        out.push(setting_finding(
            "pg_stat_statements.track",
            Severity::Medium,
            "pg_stat_statements is installed but not recording".into(),
            "pg_stat_statements.track is none, so no statement statistics accumulate: Culprit can't see which queries \
             use the time."
                .into(),
            Fix { summary: format!("Track top-level statements. {}", change.how), sql: change.sql, cost: None },
        ));
    }

    if let Some(rounds) = number(snapshot, "pg_stat_statements.dealloc").filter(|d| *d > 0.0) {
        // The rounds since the scan the count is read against (the oldest of Culprit's scans a day or more back, else
        // the previous one), an hour or more ago, when it's the same count (no reset since): what goes on now, where
        // the lifetime count can't clear after the fix.
        let since = snapshot
            .evictions_before
            .as_ref()
            .filter(|b| !b.restarted)
            .and_then(|b| {
                Some((b.taken_at.as_str(), (rounds - b.count).max(0.0), days_between(&b.taken_at, &snapshot.taken_at)?))
            })
            .filter(|(_, _, days)| *days >= 1.0 / 24.0);
        // Long enough without a round: the table has had room since, whatever the count says about before. After the
        // limit was raised, that is as long as the room it added takes to fill (`Evictions::quiet_days`).
        let evicted = Evictions::of(snapshot, rounds, since);
        let quiet = evicted.quiet_days(evicted.raised_room());
        if !since.is_some_and(|(_, new, days)| new == 0.0 && days >= quiet) {
            out.push(statements_evicted(snapshot, &evicted, since));
        }
    }

    let shared = value(snapshot, &["shared_buffers"]).and_then(bytes);
    let data = data_bytes(snapshot);
    if !snapshot.server.is_managed()
        && let Some(shared) = shared
        && shared <= 128.0 * 1024.0 * 1024.0
        && data >= 2.0 * 1024.0 * 1024.0 * 1024.0
    {
        // Medium only when reads miss it: its hit ratio counts exactly the reads shared_buffers doesn't serve.
        let missing = snapshot.pressure.as_ref().and_then(|p| p.signal("cache-hit")).is_some_and(|h| h.value < 95.0);
        let title = if (shared - 128.0 * 1024.0 * 1024.0).abs() < 1.0 {
            "shared_buffers is still the 128 MB default".to_string()
        } else {
            format!("shared_buffers is only {}", format::bytes(shared))
        };
        let mut f = setting_finding(
            "shared_buffers",
            if missing { Severity::Medium } else { Severity::Low },
            title,
            format!(
                "PostgreSQL keeps only {} of this database's {} in its own cache and leans on the operating system for the \
                 rest, which costs a copy and an extra lookup on every read.",
                format::bytes(shared),
                format::bytes(data)
            ),
            Fix {
                summary: "A common starting point is a quarter of the server's memory.".into(),
                // The server's memory isn't readable over SQL: the size is the reader's to fill in, never a guess that
                // could keep a small server from starting.
                sql: Some(
                    "-- A quarter of the server's memory (4GB on a 16 GB server); takes effect after a restart.\nALTER SYSTEM SET shared_buffers = '<a quarter of RAM>';"
                        .into(),
                ),
                cost: Some("That much memory is reserved for PostgreSQL.".into()),
            },
        );
        f.confidence = Confidence::Medium;
        after_restart(&mut f);
        out.push(f);
    }
    out
}

/// pg_stat_statements' evictions, counted. `dealloc` counts rounds, not entries: each time the table is full it drops
/// the least-used 5% of its entries, at least 10 (`entry_dealloc`), so the entries lost are about that many times the
/// rounds. A limit above its default may have been raised after some of the rounds: then the default's 5% is the floor.
#[derive(Clone, Copy)]
struct Evictions {
    rounds: f64,
    max: Option<f64>,
    default: Option<f64>,
    raised_from: Option<f64>,
    per_round: f64,
    entries: f64,
    /// The entries evicted a day now: over Culprit's scans where they cover a day and saw rounds (`since`), like a
    /// statement's runs on running totals, otherwise over the window the count covers (an hour of it at least). While
    /// the table is full, new statements arrive as fast as old ones are evicted: it is also how fast new room fills.
    per_day: Option<f64>,
    /// The entries evicted a day over the window the count covers, said beside the scans' rate where the two differ
    /// twofold. `None` where `per_day` is it.
    since_reset: Option<f64>,
}

impl Evictions {
    /// `since`: the scan the count is read against (when it was taken, the rounds since, the days between).
    fn of(snapshot: &Snapshot, rounds: f64, since: Option<(&str, f64, f64)>) -> Evictions {
        let max = number(snapshot, "pg_stat_statements.max").filter(|m| *m > 0.0);
        let default = snapshot
            .setting("pg_stat_statements.max")
            .and_then(|s| s.default_value.as_deref())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|d| *d > 0.0);
        let raised_from = default.zip(max).filter(|(d, m)| d < m).map(|(d, _)| d);
        // From a full table, so 5% of the limit; without the limit only the floor of 10 a round is known.
        let per_round = raised_from.or(max).map_or(10.0, per_round_at);
        let entries = rounds * per_round;
        // Evictions since its statistics were reset, as a rate over that same window: a burst long ago isn't today's.
        let all = crate::support::rate_per_day(snapshot, entries);
        // What Culprit's scans measured over a day or more of them, where they saw rounds: what goes on now.
        let scans = since
            .filter(|(_, new, days)| *new > 0.0 && *days >= SCANS_DAY)
            .map(|(_, new, days)| new * per_round / days);
        let (per_day, since_reset) = match scans {
            Some(now) => (Some(now), all),
            None => (all, None),
        };
        Evictions { rounds, max, default, raised_from, per_round, entries, per_day, since_reset }
    }

    /// The rate over the window the count covers, said beside the scans' (`per_day`) where the two differ twofold:
    /// " (about 2.9K a day since its statistics were reset)".
    fn beside(&self, about: &str) -> String {
        match (self.per_day, self.since_reset) {
            (Some(now), Some(all)) if now >= all * 2.0 || now * 2.0 <= all => {
                format!(" ({about} {} a day since its statistics were reset)", format::count(all))
            }
            _ => String::new(),
        }
    }

    /// The room a raise of the limit added, at most: from its default to the limit now. None when it wasn't raised.
    fn raised_room(&self) -> f64 {
        self.raised_from.zip(self.max).map_or(0.0, |(d, m)| m - d)
    }

    /// How long a later scan must see no new round before the table counts as having room: a day, or, after the limit
    /// was raised by `room` entries, as long as that room takes to fill at the rate entries are evicted now. Nothing is
    /// evicted while new room fills, whatever the statements do, so a shorter quiet span proves nothing. At most a
    /// week, the span an absence is claimed over everywhere (a weekly job has had its chance): room that takes longer
    /// fills at fewer entries a day than that, and a round after it reopens the finding.
    fn quiet_days(&self, room: f64) -> f64 {
        let fill = match self.per_day {
            _ if room <= 0.0 => 0.0,
            Some(per_day) if per_day > 0.0 => room / per_day,
            _ => 7.0,
        };
        fill.clamp(20.0 / 24.0, 7.0)
    }
}

/// Days of Culprit's scans from which an eviction count says "a day" ([`culprit_core::PER_DAY_MIN_HOURS`]).
const SCANS_DAY: f64 = culprit_core::PER_DAY_MIN_HOURS / 24.0;

/// The entries one round evicts from a full table of `limit`: the least-used 5%, at least 10.
fn per_round_at(limit: f64) -> f64 {
    (limit * 0.05).floor().max(10.0).min(limit)
}

/// "a day", "5 days", "a week": a quiet span from [`Evictions::quiet_days`].
fn quiet_span(days: f64) -> String {
    match days.ceil() {
        d if d <= 1.0 => "a day".into(),
        d if d >= 7.0 => "a week".into(),
        d => format!("{d:.0} days"),
    }
}

/// The finding about pg_stat_statements' evictions. The count runs from pg_stat_statements' last statistics reset,
/// outlives restarts and isn't cleared by a higher limit: what shows the fix is the count no longer growing. `since` is
/// the scan it is read against (when it was taken, the rounds since, the days between): with it the grade and the rate
/// are what goes on now, the rate since the reset beside it where the two differ twofold, and a quiet span without a
/// round closes the finding. Without it, after a raise the rounds may all be from before
/// it: the grade can't say they go on, and the fix is comparing two scans before raising it again.
fn statements_evicted(snapshot: &Snapshot, evicted: &Evictions, since: Option<(&str, f64, f64)>) -> Finding {
    let Evictions { rounds, max, default, raised_from, per_round, entries, per_day: entries_per_day, since_reset } =
        *evicted;
    // The rate is the scans' where they measured one (`since_reset` then holds the window's, said beside it).
    let over_scans = since_reset.is_some();
    // More room than it has now: 20,000, or twice the current size when that's already more.
    let room = max.map_or(20_000.0, |m| (m * 2.0).max(20_000.0));
    let change = setting_change(&snapshot.server, "pg_stat_statements.max", &format!("{room:.0}"));
    let estimate = max.is_some() && raised_from.is_none();
    let about = if estimate { "about" } else { "at least" };
    let workload = snapshot.workload.as_ref();
    let reset = workload
        .and_then(|w| w.window_start.as_deref())
        .map(|start| format!("since its statistics were reset on {}", format::friendly_date(start, &snapshot.taken_at)))
        .unwrap_or_else(|| "since its statistics were reset".into());
    // The entries and their evictions are the server's, every database's together; this database's own entries say
    // whether its statements are the ones filling the table. Far below the limit, they likely aren't: the evictions are
    // other databases' (or tools'), said as the server's and not graded here. A count of this database's statements
    // now against evictions over the whole window can't say whose they were then, only who likely sends them.
    let mine = workload.map(|w| w.query_count).filter(|n| *n > 0.0);
    let others = mine.zip(max).filter(|(n, limit)| *n * 4.0 <= *limit).map(|(n, _)| n);
    // Without the limit or this database's count, whose statements fill it is unknown: no cause is named.
    let fills = mine.zip(max).is_some_and(|(n, limit)| n * 4.0 > limit);
    let cause = match others {
        Some(n) => format!(
            " This database has only {} of them, so the statements filling it likely come from other databases on the \
             server, or tools connected to them; they can still push this database's rarely run statements out.",
            format::count(n)
        ),
        None if fills => {
            " This usually means the app sends many distinct statements (growing IN lists, generated SQL).".into()
        }
        None => String::new(),
    };
    // The limit as it is set ("5000"), not rounded ("5.0K").
    let limit =
        max.map_or_else(|| "its limit of distinct statements".to_string(), |m| format!("{m:.0} distinct statements"));
    let each = match (max, raised_from) {
        (Some(m), Some(d)) => format!(
            "the least-used 5% of them: {} entries at today's limit of {m:.0}, {} at its default of {d:.0}",
            format::count(per_round_at(m)),
            format::count(per_round_at(d))
        ),
        (Some(m), None) => {
            format!("the least-used 5% of them, {} entries at a limit of {m:.0}", format::count(per_round))
        }
        (None, _) => "the least-used 5% of them, at least 10 entries".into(),
    };
    // The rate over everything since the reset, where Culprit's scans measured none: theirs is said with theirs.
    let per_day = workload
        .and_then(|w| w.per_day_measured(entries))
        .filter(|_| !over_scans)
        .map(|d| format!(", {about} {} a day", format::count(d)))
        .unwrap_or_default();
    // Since the scan the count is read against (the oldest of Culprit's a day or more back, else the one before): the
    // rounds, the entries they evicted and how that reads over the time between.
    let recent = since.filter(|(_, new, _)| *new > 0.0).map(|(taken_at, new, days)| {
        let new_entries = new * per_round;
        let rate = if days >= SCANS_DAY {
            format!(", {about} {} a day{}", format::count(new_entries / days), evicted.beside(about))
        } else {
            format!(" in {}", culprit_core::span_words(days * 24.0))
        };
        let when = crate::queries::when_phrase(taken_at);
        (
            new,
            new_entries,
            new_entries / days,
            format!(
                " {} of those rounds came since Culprit's scan {when}: {about} {} entries{rate}.",
                format::count(new),
                format::count(new_entries)
            ),
            when,
        )
    });
    // Medium when this database's statements fill it and it loses at least a thousand entries a day: since the previous
    // scan where there is one. Without it, after a raise the rounds may all be from before it: what goes on now only a
    // second scan shows.
    let losing = match &recent {
        Some((_, _, a_day, _, _)) => *a_day >= 1000.0,
        None => raised_from.is_none() && entries_per_day.is_some_and(|d| d >= 1000.0),
    };
    let severity = if fills && losing { Severity::Medium } else { Severity::Low };
    let whose =
        if others.is_some() { " (pg_stat_statements without a database filter shows whose they are)" } else { "" };
    let fix = match raised_from {
        Some(d) if recent.is_some() => format!(
            "The limit is already above its default of {d:.0}, and the eviction rounds still grow between scans: keep \
             more entries, and fix the statement families that create them{whose}. {} It takes effect after a restart.",
            change.how
        ),
        Some(d) => format!(
            "The limit is already above its default of {d:.0}, and the count includes any rounds from before it was \
             raised: compare the eviction rounds between two scans first. If they still grow, keep more entries, and \
             fix the statement families that create them{whose}. {} It takes effect after a restart.",
            change.how
        ),
        None => format!(
            "Keep more entries, and fix the statement families that create them{whose}. {} It takes effect after a \
             restart.",
            change.how
        ),
    };
    let mut f = setting_finding(
        "pg_stat_statements.max",
        severity,
        match &recent {
            Some((new, new_entries, _, _, when)) => format!(
                "pg_stat_statements ran out of room {} times since Culprit's scan {when}, evicting {about} {} entries",
                format::count(*new),
                format::count(*new_entries)
            ),
            None => format!(
                "pg_stat_statements ran out of room {} times, evicting {about} {} entries",
                format::count(rounds),
                format::count(entries)
            ),
        },
        format!(
            "It keeps at most {limit} for the whole server. {}, it has run out of room {} times, and each time it \
             evicts {each}: {about} {} entries in all{per_day}.{} The numbers for anything that isn't constantly \
             running are incomplete.{cause}",
            capitalize(&reset),
            format::count(rounds),
            format::count(entries),
            recent.as_ref().map_or("", |(_, _, _, said, _)| said.as_str())
        ),
        Fix {
            summary: fix,
            sql: change.sql,
            // An entry (pgssEntry and its slot in the hash table) is about 0.4 KB on PostgreSQL 15 and grows a little as
            // versions add counters; since 9.4 the statement's text is in a file, not in shared memory.
            cost: Some(format!(
                "Under half a kilobyte of shared memory per entry, set aside when the server starts: at most about {} \
                 for {room:.0} entries. The statements' text is kept in a file on disk, not in shared memory.",
                format::bytes(room * 512.0)
            )),
        },
    );
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Eviction rounds", rounds, Unit::Count));
    if let Some((new, _, _, _, when)) = &recent {
        f.metrics.push(Metric::new(format!("Eviction rounds since Culprit's scan {when}"), *new, Unit::Count));
    }
    f.metrics.push(Metric::new(
        if estimate { "Entries evicted (estimate)" } else { "Entries evicted (at least)" },
        entries,
        Unit::Count,
    ));
    // The rounds exact: a count rounded to "15K" hides the growth this step looks for. The span between the two scans
    // is the one a later scan closes the finding on: the room above the default, which the new limit's next scan
    // reads as raised, or the room the change adds where the default isn't known.
    let added = room - default.filter(|d| *d < room).or(max).unwrap_or(0.0);
    let quiet = evicted.quiet_days(added);
    let fills = match entries_per_day {
        _ if quiet <= 20.0 / 24.0 => String::new(),
        Some(a_day) => format!(
            " The room the change adds (at most {} entries) fills first, at about {} new entries a day (the rate {}), \
             and nothing is evicted until it is full: a shorter quiet span proves nothing.{}",
            format::count(added),
            format::count(a_day),
            if over_scans { "Culprit's scans measured it evicting them" } else { "it has been evicting them" },
            if added / a_day > 7.0 { " A round after the week reopens this finding." } else { "" }
        ),
        None => {
            " How fast new statements arrive isn't known, and nothing is evicted until the room the change adds is \
                 full: a shorter quiet span proves nothing."
                .into()
        }
    };
    f.verify = vec![format!(
        "Re-scan once the change has taken effect, then {} later: the eviction rounds ({rounds:.0} now) should not grow \
         between those two scans, and with none between them this finding is gone.{fills} The count itself only starts \
         over when pg_stat_statements' statistics are reset.",
        quiet_span(quiet)
    )];
    f.caveats.push(format!(
        "The count covers everything {reset}, and neither a restart nor a higher limit clears it.{}",
        if default.is_none() {
            " If pg_stat_statements.max was raised after these evictions, compare the count between two scans before \
             raising it again."
        } else {
            ""
        }
    ));
    match (max, raised_from) {
        (Some(m), Some(_)) => f.caveats.push(format!(
            "The entries are counted at the default's {} a round, a floor: rounds after the limit was raised to {m:.0} \
             evicted {} each.",
            format::count(per_round),
            format::count(per_round_at(m))
        )),
        // Without its default, whether the limit was ever different is unknown.
        (Some(_), None) if default.is_none() => f.caveats.push(
            "The entries are estimated at today's limit: rounds counted while the limit was different evicted 5% of \
             that one."
                .into(),
        ),
        _ => {}
    }
    f
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The settings that have the server log slow statements to the table Culprit reads (and, on MariaDB, count index
/// reads), without a restart, worded where they are set ([`settings_change`]). `None` when it already does.
fn slow_log_change(snapshot: &Snapshot) -> Option<crate::support::SettingChange> {
    let mut changes: Vec<(&str, String)> = Vec::new();
    if value(snapshot, &["userstat"]).is_some_and(|v| !is_on(v)) {
        changes.push(("userstat", "ON".into()));
    }
    if value(snapshot, &["slow_query_log"]).is_some_and(|v| !is_on(v)) {
        changes.push(("slow_query_log", "ON".into()));
    }
    let output = value(snapshot, &["log_output"]).unwrap_or("FILE").trim().to_ascii_uppercase();
    if !output.split(',').any(|o| o.trim() == "TABLE") {
        let output =
            if output.is_empty() || output == "NONE" { "TABLE".to_string() } else { format!("{output},TABLE") };
        changes.push(("log_output", output));
    }
    if number(snapshot, "long_query_time").is_none_or(|t| t > 1.0) {
        changes.push(("long_query_time", "0.1".into()));
    }
    let changes: Vec<(&str, &str)> = changes.iter().map(|(n, v)| (*n, v.as_str())).collect();
    (!changes.is_empty()).then(|| settings_change(&snapshot.server, &changes))
}

/// Settings read only at startup: the server configuration (my.cnf) and a restart on a server you manage; the
/// platform's control and a reboot on a managed one.
fn restart_change(snapshot: &Snapshot, changes: &[(&str, &str)]) -> crate::support::SettingChange {
    let managed = settings_change(&snapshot.server, changes);
    if managed.sql.is_none() {
        return crate::support::SettingChange {
            how: format!("{} It takes effect after a reboot.", managed.how),
            sql: None,
        };
    }
    let lines: Vec<String> = changes.iter().map(|(n, v)| format!("{n} = {v}")).collect();
    crate::support::SettingChange {
        how: "Set it in the server configuration (my.cnf) and restart.".into(),
        sql: Some(format!("-- In the server configuration (my.cnf), then restart:\n[mysqld]\n{}", lines.join("\n"))),
    }
}

pub(crate) fn mysql(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let name = snapshot.server.name();
    if value(snapshot, &["performance_schema"]).is_some_and(|v| !is_on(v)) {
        let logged = snapshot.workload.as_ref().is_some_and(|w| w.kind() == Some(culprit_core::WorkloadKind::SlowLog));
        let (severity, summary) = if logged {
            (
                Severity::Medium,
                format!(
                    "Culprit reads the slow query log, so statements slower than long_query_time are covered. Without \
                     performance_schema, {name} keeps no statistics for faster statements and no wait statistics."
                ),
            )
        } else {
            (
                Severity::High,
                format!(
                    "Without it {name} keeps no statement digests, index usage or wait statistics: there is nothing to \
                     tell which queries are slow or which indexes are used."
                ),
            )
        };
        let restart = restart_change(snapshot, &[("performance_schema", "ON")]);
        let (fix_summary, sql) = match slow_log_change(snapshot).filter(|_| !logged) {
            Some(now) => (
                format!(
                    "Start with the slow query log, which needs no restart: Culprit reads it from the mysql.slow_log table. {} \
                     Turn performance_schema on at the next restart for statistics on every statement: {}",
                    now.how,
                    lowercase_first(&restart.how)
                ),
                match (now.sql, restart.sql) {
                    (Some(now), Some(restart)) => Some(format!(
                        "-- Now, no restart (connections opened earlier keep the old threshold):\n{now}\n\n{restart}"
                    )),
                    (now, restart) => now.or(restart),
                },
            ),
            None => (format!("Turn it on (it can't be changed at runtime). {}", restart.how), restart.sql),
        };
        let mut f = setting_finding(
            "performance_schema",
            severity,
            "performance_schema is off".into(),
            summary,
            Fix {
                summary: fix_summary,
                sql,
                cost: Some("A few hundred MB of memory and a small CPU overhead.".into()),
            },
        );
        f.confidence = Confidence::High;
        after_restart(&mut f);
        out.push(f);
        return out;
    }
    if value(snapshot, &["performance_schema.consumer.statements_digest"]).is_some_and(|v| v.eq_ignore_ascii_case("NO"))
    {
        out.push(setting_finding(
            "statements_digest",
            Severity::Medium,
            "Statement digests aren't being collected".into(),
            "The statements_digest consumer is disabled, so performance_schema doesn't aggregate statements: the workload \
             view stays empty."
                .into(),
            Fix {
                summary: "Enable the consumer (and persist it in my.cnf).".into(),
                sql: Some("UPDATE performance_schema.setup_consumers SET ENABLED = 'YES' WHERE NAME = 'statements_digest';".into()),
                cost: None,
            },
        ));
    }
    let lost = number(snapshot, "performance_schema_digest_lost").unwrap_or(0.0);
    let untracked_ms = number(snapshot, "performance_schema.untracked_ms").unwrap_or(0.0);
    let total_ms = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
    if lost > 0.0 || (total_ms > 0.0 && untracked_ms / (total_ms + untracked_ms) >= 0.05) {
        let size = value(snapshot, &["performance_schema_digests_size"]).unwrap_or("10000").to_string();
        let untracked_share =
            if total_ms + untracked_ms > 0.0 { untracked_ms / (total_ms + untracked_ms) } else { 0.0 };
        // More room than it has now (-1 is "sized automatically"): 50,000, or twice what it holds.
        let room = size.trim().parse::<f64>().ok().filter(|s| *s > 0.0).map_or(50_000.0, |s| (s * 2.0).max(50_000.0));
        let room = format!("{room:.0}");
        let change = restart_change(snapshot, &[("performance_schema_digests_size", room.as_str())]);
        let mut f = setting_finding(
            "performance_schema_digests_size",
            if untracked_share >= 0.2 { Severity::Medium } else { Severity::Low },
            "performance_schema ran out of room for statement digests".into(),
            format!(
                "Its digest table holds {size} statements and is full: {} new statements were lumped into one anonymous \
                 row{}. Those statements are invisible to per-query analysis. It usually means the app sends many \
                 distinct statements (generated SQL, unparameterized literals).",
                format::count(lost),
                if untracked_share > 0.0 {
                    format!(", which holds {} of statement time", format::percent(untracked_share))
                } else {
                    String::new()
                }
            ),
            Fix {
                summary: format!("Raise the table size and truncate the summary to start fresh. {}", change.how),
                sql: Some(match change.sql {
                    Some(restart) => format!(
                        "{restart}\n-- or start over now:\nTRUNCATE TABLE performance_schema.events_statements_summary_by_digest;"
                    ),
                    None => {
                        "-- Start over now:\nTRUNCATE TABLE performance_schema.events_statements_summary_by_digest;"
                            .into()
                    }
                }),
                cost: None,
            },
        );
        f.confidence = Confidence::High;
        // Digests lost is a status counter: a restart starts it over; after a truncate alone, its growth is what tells.
        f.verify = vec![
            "Re-scan after the restart: no digests lost, and no statement time left in the anonymous row. After a \
             truncate alone, compare the lost count between two scans: it should stop growing."
                .into(),
        ];
        f.metrics.push(Metric::new("Digests lost", lost, Unit::Count));
        if untracked_share > 0.0 {
            f.metrics.push(Metric::new("Untracked time", untracked_share, Unit::Ratio));
        }
        out.push(f);
    }
    let truncated = snapshot.queries().iter().filter(|q| q.text.trim_end().ends_with("...")).count();
    if truncated >= 3 && number(snapshot, "max_digest_length").is_some_and(|v| v <= 1024.0) {
        let change = restart_change(
            snapshot,
            &[
                ("max_digest_length", "4096"),
                ("performance_schema_max_digest_length", "4096"),
                ("performance_schema_max_sql_text_length", "4096"),
            ],
        );
        let mut f = setting_finding(
            "max_digest_length",
            Severity::Low,
            format!("{truncated} statements are cut off at 1,024 characters"),
            "Long statements are truncated in performance_schema, so statements that differ only after the cut are \
             merged into one digest, and Culprit can't explain them."
                .into(),
            Fix {
                summary: format!("Keep longer text. {}", change.how),
                sql: change.sql,
                cost: Some("A little more memory per digest.".into()),
            },
        );
        f.confidence = Confidence::Medium;
        // The statements already cut off stay as they were recorded: only those recorded after the restart are whole.
        f.verify = vec![
            "Re-scan after the server has restarted with the change and the statements have run again: they are no \
             longer cut off, and this finding is gone."
                .into(),
        ];
        out.push(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waits::snapshot_with;
    use culprit_core::{Pressure, Setting, TableStorage};

    fn set(s: &mut Snapshot, name: &str, value: &str) {
        s.settings.retain(|x| !x.name.eq_ignore_ascii_case(name));
        s.settings.push(Setting {
            name: name.into(),
            value: value.into(),
            default_value: None,
            scope: "server".into(),
        });
    }

    fn found<'a>(findings: &'a [Finding], id: &str) -> Option<&'a Finding> {
        findings.iter().find(|f| f.id == id)
    }

    // C9: with size-based cleanup on AUTO, Query Store trims itself at 90% and never fills.
    #[test]
    fn a_query_store_that_cleans_itself_is_never_too_full() {
        let mut s = snapshot_with(Engine::Mssql);
        set(&mut s, "query_store_options.current_storage_size_mb", "950");
        set(&mut s, "query_store_options.max_storage_size_mb", "1000");
        set(&mut s, "query_store_options.size_based_cleanup_mode_desc", "AUTO");
        assert!(found(&mssql(&s), "config:query-store-size").is_none());
        set(&mut s, "query_store_options.size_based_cleanup_mode_desc", "OFF");
        assert!(found(&mssql(&s), "config:query-store-size").is_some());
    }

    // C6: MAXDOP 0 is Microsoft's own advice up to 8 processors; above that the cap fits one NUMA node.
    #[test]
    fn maxdop_zero_matters_only_on_a_bigger_server() {
        let mut s = snapshot_with(Engine::Mssql);
        set(&mut s, "max degree of parallelism", "0");
        set(&mut s, "MAXDOP", "0");
        s.pressure = None;
        assert!(found(&mssql(&s), "config:maxdop").is_none(), "unknown processor count: nothing to judge");
        s.pressure = Some(Pressure { cpus: Some(4.0), numa_nodes: Some(1.0), ..Pressure::default() });
        assert!(found(&mssql(&s), "config:maxdop").is_none(), "4 processors: MAXDOP 0 is fine");
        s.pressure = Some(Pressure { cpus: Some(24.0), numa_nodes: Some(2.0), ..Pressure::default() });
        let f = found(&mssql(&s), "config:maxdop").cloned().unwrap();
        assert!(f.title.contains("all 24 processors"), "{}", f.title);
        assert!(f.fix.unwrap().sql.unwrap().contains("MAXDOP = 8"));
        s.pressure = Some(Pressure { cpus: Some(12.0), numa_nodes: Some(2.0), ..Pressure::default() });
        assert!(found(&mssql(&s), "config:maxdop").cloned().unwrap().fix.unwrap().sql.unwrap().contains("MAXDOP = 6"));
    }

    // C16: only big tables that churn and follow the server's factor; the title says the factor in use.
    #[test]
    fn autovacuum_scale_skips_append_only_tables_and_their_own_settings() {
        let mut s = snapshot_with(Engine::Postgres);
        set(&mut s, "autovacuum_vacuum_scale_factor", "0.3");
        for t in &mut s.tables {
            t.rows = Some(1_000.0);
        }
        let t = &mut s.tables[0];
        t.rows = Some(20_000_000.0);
        t.updates = Some(0.0);
        t.deletes = Some(0.0);
        assert!(found(&postgres(&s), "config:autovacuum-scale").is_none(), "append-only: no dead rows to wait for");
        s.tables[0].updates = Some(2_000_000.0);
        let f = found(&postgres(&s), "config:autovacuum-scale").cloned().unwrap();
        assert!(f.title.starts_with("Autovacuum waits for 30% of"), "{}", f.title);
        s.tables[0].storage =
            Some(TableStorage { options: vec!["autovacuum_vacuum_scale_factor=0.01".into()], ..Default::default() });
        assert!(found(&postgres(&s), "config:autovacuum-scale").is_none(), "it has its own factor");
    }

    // C7: ALTER SYSTEM is refused on managed PostgreSQL; never propose a smaller pg_stat_statements.max.
    #[test]
    fn postgres_settings_are_worded_where_they_run() {
        let mut s = snapshot_with(Engine::Postgres);
        set(&mut s, "autovacuum", "off");
        set(&mut s, "pg_stat_statements.dealloc", "40");
        set(&mut s, "pg_stat_statements.max", "50000");
        let out = postgres(&s);
        assert!(
            found(&out, "config:autovacuum")
                .unwrap()
                .fix
                .as_ref()
                .unwrap()
                .sql
                .as_deref()
                .unwrap()
                .starts_with("ALTER SYSTEM SET autovacuum")
        );
        let max = found(&out, "config:pg_stat_statements.max").unwrap().fix.clone().unwrap();
        assert!(max.sql.unwrap().contains("pg_stat_statements.max = 100000"));
        s.server.cloud = Some("Azure Database for PostgreSQL".into());
        let fix = found(&postgres(&s), "config:autovacuum").unwrap().fix.clone().unwrap();
        assert!(fix.sql.is_none() && fix.summary.contains("server parameters"), "{}", fix.summary);
    }

    // C7: with performance_schema off on RDS, the fix is the parameter group, not SET GLOBAL and my.cnf.
    #[test]
    fn mysql_restart_settings_on_a_managed_server() {
        let mut s = snapshot_with(Engine::Mysql);
        set(&mut s, "performance_schema", "OFF");
        set(&mut s, "slow_query_log", "OFF");
        let fix = found(&mysql(&s), "config:performance_schema").unwrap().fix.clone().unwrap();
        assert!(fix.sql.as_deref().unwrap().contains("[mysqld]\nperformance_schema = ON"));
        s.server.cloud = Some("Amazon RDS".into());
        let fix = found(&mysql(&s), "config:performance_schema").unwrap().fix.clone().unwrap();
        assert!(fix.sql.is_none(), "{:?}", fix.sql);
        assert!(fix.summary.contains("in the instance's DB parameter group"), "{}", fix.summary);
        assert!(fix.summary.contains("after a reboot"));
    }
}
