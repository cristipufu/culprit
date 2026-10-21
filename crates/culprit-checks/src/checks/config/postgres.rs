//! PostgreSQL configuration and `pg_stat_statements` eviction checks.

use super::*;

pub(super) fn check(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = settings(snapshot);
    out.extend(health(snapshot));
    out
}

fn settings(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    // Planner cost constants are a model of this installation's cache and storage. A default ratio, including on
    // managed storage, proves no slow plan and justifies no database-wide change. Keep them visible in settings.
    if value(snapshot, &["track_io_timing"]).is_some_and(|v| !is_on(v)) {
        let change = setting_change(&snapshot.server, "track_io_timing", "on");
        let mut f = setting_finding(
            "track_io_timing",
            Severity::Low,
            "I/O timing isn't tracked".into(),
            "With track_io_timing off, pg_stat_statements and EXPLAIN (BUFFERS) can't tell how much of a query's time is spent waiting on disk.".into(),
            Fix {
                summary: format!("Turn it on; the overhead is negligible on modern systems. {}", change.how),
                sql: change.sql,
                cost: None,
            },
        );
        f.impact.score = 4.0;
        out.push(f);
    }
    out
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

fn health(snapshot: &Snapshot) -> Vec<Finding> {
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
        // A miss can be an operating-system cache hit. Only this database's measured I/O time establishes a cost.
        let io_ms: f64 = snapshot.waits.iter().filter(|w| category_key(&w.category) == "io").map(|w| w.wait_ms).sum();
        let runtime = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
        let slow_io = runtime > 0.0 && severity_for_time(snapshot, io_ms, io_ms / runtime) >= Severity::Medium;
        let title = if (shared - 128.0 * 1024.0 * 1024.0).abs() < 1.0 {
            "shared_buffers is still the 128 MB default".to_string()
        } else {
            format!("shared_buffers is only {}", format::bytes(shared))
        };
        let mut f = setting_finding(
            "shared_buffers",
            if slow_io { Severity::Medium } else { Severity::Low },
            title,
            format!(
                "PostgreSQL keeps only {} of this database's {} in its own cache and leans on the operating system for the \
                 rest. Operating-system cache hits can still be fast; this size and the cache-hit ratio alone do not establish a memory shortage.",
                format::bytes(shared),
                format::bytes(data)
            ),
            Fix {
                summary: "Measure this database's read latency and I/O time during normal load before changing shared_buffers. If they confirm substantial I/O, a common starting point is a quarter of the server's memory, with room left for the operating system and query memory.".into(),
                // The server's memory isn't readable over SQL: the size is the reader's to fill in, never a guess that
                // could keep a small server from starting.
                sql: slow_io.then(||
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

    if let Some(full) = snapshot.pressure.as_ref().and_then(|p| p.signal("wal-buffers-full")).map(|s| s.value)
        && counter_rate_per_day(snapshot, full).is_some_and(|r| r >= 24_000.0)
        && value(snapshot, &["wal_buffers"]).and_then(bytes).is_none_or(|b| b < 64.0 * 1024.0 * 1024.0)
    {
        let current = value(snapshot, &["wal_buffers"]).unwrap_or("-1 (auto-sized, up to 16 MB)");
        let change = setting_change(&snapshot.server, "wal_buffers", "64MB");
        let mut f = setting_finding(
            "wal_buffers",
            Severity::Low,
            format!("WAL buffers filled {} times {}", format::count(full), since_phrase(snapshot)),
            format!(
                "wal_buffers is {current}, and backends had to flush WAL buffer pages themselves {} times {} \
                 (pg_stat_wal.wal_buffers_full) because the buffers filled before walwriter wrote them out. \
                 Each full-buffer write makes the backend wait on WAL I/O during bursts of writes.",
                format::count(full),
                since_phrase(snapshot)
            ),
            Fix {
                summary: format!(
                    "Give walwriter more buffer room (64 MB is four WAL segments). {} It takes effect after a restart.",
                    change.how
                ),
                sql: change.sql,
                cost: Some("64 MB of shared memory.".into()),
            },
        );
        f.confidence = Confidence::Medium;
        f.metrics.push(Metric::new("WAL buffer full writes", full, Unit::Count));
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

/// The finding about pg_stat_statements' evictions.
fn statements_evicted(snapshot: &Snapshot, evicted: &Evictions, since: Option<(&str, f64, f64)>) -> Finding {
    let Evictions { rounds, max, default, raised_from, per_round, entries, per_day: entries_per_day, since_reset } =
        *evicted;
    let over_scans = since_reset.is_some();
    let room = max.map_or(20_000.0, |m| (m * 2.0).max(20_000.0));
    let change = setting_change(&snapshot.server, "pg_stat_statements.max", &format!("{room:.0}"));
    let estimate = max.is_some() && raised_from.is_none();
    let about = if estimate { "about" } else { "at least" };
    let workload = snapshot.workload.as_ref();
    let reset = workload
        .and_then(|w| w.window_start.as_deref())
        .map(|start| format!("since its statistics were reset on {}", format::friendly_date(start, &snapshot.taken_at)))
        .unwrap_or_else(|| "since its statistics were reset".into());
    let mine = workload.map(|w| w.query_count).filter(|n| *n > 0.0);
    let others = mine.zip(max).filter(|(n, limit)| *n * 4.0 <= *limit).map(|(n, _)| n);
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
    let per_day = workload
        .and_then(|w| w.per_day_measured(entries))
        .filter(|_| !over_scans)
        .map(|d| format!(", {about} {} a day", format::count(d)))
        .unwrap_or_default();
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
