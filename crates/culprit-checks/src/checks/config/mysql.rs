//! MySQL and MariaDB configuration, `performance_schema`, buffer pool, and slow query log checks.

use super::*;

pub(crate) const MARIADB_TABLE_LOCK_TIME: &str = "Written to a table (TABLE in log_output), MariaDB counts the whole run of each statement it logs as that statement's lock time, so their lock time says nothing (Culprit reads it as unknown); written to a file (FILE), it is the real wait.";

pub(super) fn check(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = settings(snapshot);
    out.extend(health(snapshot));
    out
}

/// Bytes of this database's InnoDB tables: those with a clustered primary key (InnoDB's), and those without a primary
/// key (InnoDB is the default engine). A table whose primary key isn't clustered is another engine's (MyISAM) and has
/// no pages in the buffer pool.
fn innodb_bytes(snapshot: &Snapshot) -> f64 {
    snapshot
        .tables
        .iter()
        .filter(|t| {
            !t.has_primary_key || snapshot.indexes_on(&t.schema, &t.name).any(|i| i.is_primary_key && i.is_clustered)
        })
        .filter_map(|t| t.total_bytes)
        .sum()
}

fn settings(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(f) = buffer_pool(snapshot) {
        out.push(f);
    }
    // With performance_schema off, its finding already leads with the slow query log.
    let ps_off = value(snapshot, &["performance_schema"]).is_some_and(|v| !is_on(v));
    if value(snapshot, &["slow_query_log"]).is_some_and(|v| !is_on(v)) && !ps_off {
        let output = value(snapshot, &["log_output"]).unwrap_or("FILE").trim().to_ascii_uppercase();
        let output = (!output.split(',').any(|o| o.trim() == "TABLE")).then(|| {
            if output.is_empty() || output == "NONE" { "TABLE".to_string() } else { format!("{output},TABLE") }
        });
        // A second is the threshold to suggest, never a raise: one already lower (0.2 s) logs more and stays.
        let lower_already =
            value(snapshot, &["long_query_time"]).and_then(|v| v.trim().parse::<f64>().ok()).is_some_and(|t| t <= 1.0);
        let mut changes = vec![("slow_query_log", "ON")];
        if !lower_already {
            changes.push(("long_query_time", "1"));
        }
        if let Some(output) = output.as_deref() {
            changes.push(("log_output", output));
        }
        let change = settings_change(&snapshot.server, &changes);
        let mut f = setting_finding(
            "slow_query_log",
            Severity::Low,
            "The slow query log is off".into(),
            "Without it there is no record of individual slow statements with their real parameter values, only the aggregated digests. Logged to the mysql.slow_log table, Culprit also uses those statements to plan digests that have no sample text.".into(),
            Fix {
                summary: format!(
                    "Turn it on with a threshold that matches your latency budget. {}{}",
                    change.how,
                    if lower_already {
                        ""
                    } else {
                        " The new long_query_time applies to new connections: reopen the application's existing connections, or have the application set the session variable on them."
                    }
                ),
                sql: change.sql,
                // What the table costs the lock time every check reads: the reason Culprit asks for it said beside it.
                cost: snapshot.server.is_mariadb().then(|| {
                    format!(
                        "{MARIADB_TABLE_LOCK_TIME} Leaving TABLE out keeps lock time real, but Culprit then can't use the logged statements to plan digests without sample text."
                    )
                }),
            },
        );
        f.impact.score = 4.0;
        out.push(f);
    }
    out
}

/// A buffer pool smaller than the data is a problem only when reads miss it: a large, mostly cold schema served
/// from memory 99.9% of the time needs nothing. Graded on the hit rate and how much it reads from disk, since the
/// server started (at least an hour of it). The server's memory isn't known, so no size is proposed.
pub(super) fn buffer_pool(snapshot: &Snapshot) -> Option<Finding> {
    let pool =
        value(snapshot, &["innodb_buffer_pool_size"]).and_then(|v| v.parse::<f64>().ok()).filter(|p| *p > 0.0)?;
    let data = innodb_bytes(snapshot);
    if data < 1024.0 * 1024.0 * 1024.0 || pool >= data * 0.5 {
        return None;
    }
    counter_days_for_rate(snapshot)?;
    let pressure = snapshot.pressure.as_ref()?;
    let hit = pressure.signal(culprit_core::signal_id::BUFFER_POOL_HIT).map(|s| s.value)?;
    if hit >= 99.0 {
        return None;
    }
    let reads = pressure.signal(culprit_core::signal_id::BUFFER_POOL_READS).map(|s| s.value);
    // Both counters cover all databases; neither measures this database's delays.
    let severity = Severity::Low;
    let name = snapshot.server.name();
    let mut f = setting_finding(
        "innodb_buffer_pool_size",
        severity,
        format!("The server's InnoDB buffer pool records {} of reads missing memory", format::percent((100.0 - hit) / 100.0)),
        format!(
            "innodb_buffer_pool_size is {}, and this database has {} of InnoDB tables and indexes. Since {name} started, {} of page reads across the whole server were served from its buffer pool{}. The rest may have been served by the operating system cache; this does not establish slow reads in this database.",
            format::bytes(pool),
            format::bytes(data),
            format::percent(hit / 100.0),
            reads.map(|r| format!(" (about {} pages a second from disk)", format::count(r))).unwrap_or_default()
        ),
        Fix {
            summary: match snapshot.server.platform() {
                Some(platform) => format!(
                    "If measured read latency and this database's workload confirm a memory shortage, consider giving the buffer pool more of the instance's memory{} ({} sizes it from the instance's memory), or move to an instance with more memory. Leave room for connections and the operating system.",
                    platform.settings_control().map(|c| format!(" in {c}")).unwrap_or_default(),
                    platform.label(snapshot.server.engine)
                ),
                None => "Check measured read latency and this database's workload before resizing the server's buffer pool. If they confirm a memory shortage, allow more memory for the pool while leaving room for connections and the operating system; it can be resized online with SET GLOBAL innodb_buffer_pool_size.".into(),
            },
            sql: None,
            cost: Some("Uses that much RAM; a pool larger than the free memory makes the server swap or be killed.".into()),
        },
    );
    // The hit ratio counts since the server started: a pool resized online still carries its misses from before.
    f.verify = vec![format!(
        "Re-scan after a day of normal load: reads served from memory should be at 99% or more ({} now). The share \
         counts since {name} started, so after an online resize it rises slowly; after a restart it counts afresh.",
        format::percent(hit / 100.0)
    )];
    f.metrics.push(Metric::new("Reads served from memory", hit / 100.0, Unit::Ratio));
    if let Some(r) = reads {
        f.metrics.push(Metric::new("Pages read from disk per second", r, Unit::Count));
    }
    f.confidence = Confidence::Medium;
    Some(f)
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
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

fn health(snapshot: &Snapshot) -> Vec<Finding> {
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
    if let Some(waits) = snapshot.pressure.as_ref().and_then(|p| p.signal("buffer-pool-wait-free")).map(|s| s.value)
        && counter_rate_per_day(snapshot, waits).is_some_and(|r| r >= 240.0)
    {
        let mut f = setting_finding(
            "innodb_buffer_pool_wait_free",
            if waits >= 1_000.0 { Severity::Medium } else { Severity::Low },
            format!("Queries waited {} times for a clean buffer-pool page", format::count(waits)),
            format!(
                "Innodb_buffer_pool_wait_free is {} {}: foreground threads needed a free page in the InnoDB buffer \
                 pool when none was clean, and had to wait for dirty pages to be flushed first.",
                format::count(waits),
                since_phrase(snapshot)
            ),
            Fix {
                summary:
                    "If the buffer pool is undersized for the working set, grow innodb_buffer_pool_size; if write \
                          bursts dirty pages faster than background flushing cleans them, raise innodb_io_capacity \
                          (and innodb_io_capacity_max) to match the storage's IOPS."
                        .into(),
                sql: None,
                cost: None,
            },
        );
        f.confidence = Confidence::Medium;
        f.metrics.push(Metric::new("Buffer pool free-page waits", waits, Unit::Count));
        f.verify = vec![
            "Compare Innodb_buffer_pool_wait_free between two scans under normal write load: it should stop growing."
                .into(),
        ];
        out.push(f);
    }
    out
}
