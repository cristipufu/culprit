//! Settings that hide the data Culprit (and you) need, or leave obvious performance on the table, per engine: Query
//! Store health on SQL Server, pg_stat_statements and memory/autovacuum defaults on Postgres, performance_schema on
//! MySQL. Part of the `config` check.

use culprit_core::{Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, Unit, format};

use crate::config::{is_on, setting_finding, value};
use crate::support::quote;

fn number(snapshot: &Snapshot, name: &str) -> Option<f64> {
    value(snapshot, &[name]).and_then(|v| v.trim().parse::<f64>().ok())
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

    if qs_on
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
        let mut f = setting_finding(
            "query-store-capture",
            if busy { Severity::Medium } else { Severity::Low },
            "Query Store records every statement, including one-off ones".into(),
            format!(
                "Capture mode ALL keeps statements that ran once or cost nothing{}. They crowd the store, shorten how far \
                 back it can look and add overhead; AUTO keeps what matters.",
                snapshot
                    .workload
                    .as_ref()
                    .map(|w| format!(" (it tracks {} statements now)", format::count(w.query_count)))
                    .unwrap_or_default()
            ),
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

    let server_maxdop = number(snapshot, "max degree of parallelism");
    let db_maxdop = number(snapshot, "MAXDOP");
    if snapshot.server.cloud.is_none() && server_maxdop == Some(0.0) && db_maxdop.is_none_or(|v| v == 0.0) {
        let mut f = setting_finding(
            "maxdop",
            Severity::Low,
            "One query may use every CPU core (MAXDOP 0)".into(),
            "With no limit on parallelism, a single large query can occupy all schedulers and make everything else \
             queue behind it. Microsoft's guidance is at most 8, or the cores in one NUMA node."
                .into(),
            Fix {
                summary: "Cap it for this database (or server-wide with sp_configure).".into(),
                sql: Some("ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = 8;".into()),
                cost: None,
            },
        );
        f.confidence = Confidence::Low;
        out.push(f);
    }
    out
}

pub(crate) fn postgres(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    if value(snapshot, &["autovacuum"]).is_some_and(|v| !is_on(v)) {
        let mut f = setting_finding(
            "autovacuum",
            Severity::High,
            "Autovacuum is off".into(),
            "Dead rows from updates and deletes are never cleaned up automatically, tables and indexes keep growing, \
             planner statistics go stale, and transaction-ID wraparound becomes a real risk."
                .into(),
            Fix {
                summary: "Turn it back on; tune it per table instead of disabling it.".into(),
                sql: Some("ALTER SYSTEM SET autovacuum = on;\nSELECT pg_reload_conf();".into()),
                cost: None,
            },
        );
        f.confidence = Confidence::High;
        out.push(f);
    }

    let big: Vec<&culprit_core::TableStat> =
        snapshot.tables.iter().filter(|t| t.rows.unwrap_or(0.0) >= 10_000_000.0).collect();
    if !big.is_empty() && number(snapshot, "autovacuum_vacuum_scale_factor").is_some_and(|v| v >= 0.2) {
        let biggest =
            big.iter().max_by(|a, b| a.rows.unwrap_or(0.0).total_cmp(&b.rows.unwrap_or(0.0))).expect("non-empty");
        let mut f = setting_finding(
            "autovacuum-scale",
            Severity::Low,
            format!("Autovacuum waits for 20% of {} to change before cleaning it", biggest.name),
            format!(
                "autovacuum_vacuum_scale_factor is {}: {} ({} rows) is only vacuumed after about {} rows change, so \
                 dead rows pile up between runs and each run is long.",
                value(snapshot, &["autovacuum_vacuum_scale_factor"]).unwrap_or("0.2"),
                biggest.qualified(),
                format::count(biggest.rows.unwrap_or(0.0)),
                format::count(biggest.rows.unwrap_or(0.0) * 0.2)
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
        out.push(f);
    }

    if value(snapshot, &["pg_stat_statements.track"]).is_some_and(|v| v.eq_ignore_ascii_case("none")) {
        out.push(setting_finding(
            "pg_stat_statements.track",
            Severity::Medium,
            "pg_stat_statements is installed but not recording".into(),
            "pg_stat_statements.track is none, so no statement statistics accumulate: Culprit can't see which queries \
             use the time."
                .into(),
            Fix {
                summary: "Track top-level statements.".into(),
                sql: Some("ALTER SYSTEM SET pg_stat_statements.track = 'top';\nSELECT pg_reload_conf();".into()),
                cost: None,
            },
        ));
    }

    if let Some(dealloc) = number(snapshot, "pg_stat_statements.dealloc").filter(|d| *d > 0.0) {
        let max = value(snapshot, &["pg_stat_statements.max"]).unwrap_or("5000").to_string();
        let mut f = setting_finding(
            "pg_stat_statements.max",
            if dealloc >= 1000.0 { Severity::Medium } else { Severity::Low },
            format!("pg_stat_statements forgot statements {} times to make room", format::count(dealloc)),
            format!(
                "It keeps at most {max} distinct statements and has evicted the least-used ones {} times since its \
                 statistics were reset, so the numbers for anything that isn't constantly running are incomplete. This \
                 usually means the app sends many distinct statements (growing IN lists, generated SQL).",
                format::count(dealloc)
            ),
            Fix {
                summary: "Keep more entries (needs a restart), and fix the statement families that create them.".into(),
                sql: Some("ALTER SYSTEM SET pg_stat_statements.max = 20000; -- takes effect after a restart".into()),
                cost: Some("About 2 KB of shared memory per entry.".into()),
            },
        );
        f.confidence = Confidence::High;
        f.metrics.push(Metric::new("Evictions", dealloc, Unit::Count));
        out.push(f);
    }

    let shared = value(snapshot, &["shared_buffers"]).and_then(bytes);
    let data = data_bytes(snapshot);
    if snapshot.server.cloud.is_none()
        && let Some(shared) = shared
        && shared <= 128.0 * 1024.0 * 1024.0
        && data >= 2.0 * 1024.0 * 1024.0 * 1024.0
    {
        let mut f = setting_finding(
            "shared_buffers",
            Severity::Medium,
            "shared_buffers is still the 128 MB default".into(),
            format!(
                "PostgreSQL keeps only {} of this database's {} in its own cache and leans on the operating system for the \
                 rest, which costs a copy and an extra lookup on every read.",
                format::bytes(shared),
                format::bytes(data)
            ),
            Fix {
                summary: "A common starting point is a quarter of the server's memory.".into(),
                sql: Some(
                    "ALTER SYSTEM SET shared_buffers = '4GB'; -- size to ~25% of RAM; takes effect after a restart"
                        .into(),
                ),
                cost: Some("That much memory is reserved for PostgreSQL.".into()),
            },
        );
        f.confidence = Confidence::Medium;
        out.push(f);
    }
    out
}

/// `SET GLOBAL …` that has the server log slow statements to the table Culprit reads (and, on MariaDB, count index
/// reads), without a restart. `None` when it already does.
fn slow_log_statement(snapshot: &Snapshot) -> Option<String> {
    let mut parts = Vec::new();
    if value(snapshot, &["userstat"]).is_some_and(|v| !is_on(v)) {
        parts.push("GLOBAL userstat = ON".to_string());
    }
    if value(snapshot, &["slow_query_log"]).is_some_and(|v| !is_on(v)) {
        parts.push("GLOBAL slow_query_log = ON".to_string());
    }
    let output = value(snapshot, &["log_output"]).unwrap_or("FILE").trim().to_ascii_uppercase();
    if !output.split(',').any(|o| o.trim() == "TABLE") {
        let output =
            if output.is_empty() || output == "NONE" { "TABLE".to_string() } else { format!("{output},TABLE") };
        parts.push(format!("GLOBAL log_output = '{output}'"));
    }
    if number(snapshot, "long_query_time").is_none_or(|t| t > 1.0) {
        parts.push("GLOBAL long_query_time = 0.1".to_string());
    }
    (!parts.is_empty()).then(|| format!("SET {};", parts.join(", ")))
}

pub(crate) fn mysql(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let name = snapshot.server.name();
    if value(snapshot, &["performance_schema"]).is_some_and(|v| !is_on(v)) {
        let logged = snapshot.workload.as_ref().is_some_and(|w| w.source == "slow query log");
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
        let restart = "-- In the server configuration (my.cnf), then restart:\n[mysqld]\nperformance_schema = ON";
        let (fix_summary, sql) = match slow_log_statement(snapshot).filter(|_| !logged) {
            Some(now) => (
                "Start with the slow query log, which needs no restart: Culprit reads it from the mysql.slow_log table. \
                 Turn performance_schema on at the next restart for statistics on every statement."
                    .to_string(),
                format!("-- Now, no restart (connections opened earlier keep the old threshold):\n{now}\n\n{restart}"),
            ),
            None => ("Turn it on in the server configuration (it can't be changed at runtime).".to_string(), restart.to_string()),
        };
        let mut f = setting_finding(
            "performance_schema",
            severity,
            "performance_schema is off".into(),
            summary,
            Fix {
                summary: fix_summary,
                sql: Some(sql),
                cost: Some("A few hundred MB of memory and a small CPU overhead.".into()),
            },
        );
        f.confidence = Confidence::High;
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
                summary: "Raise the table size (needs a restart) and truncate the summary to start fresh.".into(),
                sql: Some("-- my.cnf, then restart:\n[mysqld]\nperformance_schema_digests_size = 50000\n-- or start over now:\nTRUNCATE TABLE performance_schema.events_statements_summary_by_digest;".into()),
                cost: None,
            },
        );
        f.confidence = Confidence::High;
        f.metrics.push(Metric::new("Digests lost", lost, Unit::Count));
        if untracked_share > 0.0 {
            f.metrics.push(Metric::new("Untracked time", untracked_share, Unit::Ratio));
        }
        out.push(f);
    }
    let truncated = snapshot.queries().iter().filter(|q| q.text.trim_end().ends_with("...")).count();
    if truncated >= 3 && number(snapshot, "max_digest_length").is_some_and(|v| v <= 1024.0) {
        let mut f = setting_finding(
            "max_digest_length",
            Severity::Low,
            format!("{truncated} statements are cut off at 1,024 characters"),
            "Long statements are truncated in performance_schema, so statements that differ only after the cut are \
             merged into one digest, and Culprit can't explain them."
                .into(),
            Fix {
                summary: "Keep longer text (needs a restart).".into(),
                sql: Some("-- my.cnf, then restart:\n[mysqld]\nmax_digest_length = 4096\nperformance_schema_max_digest_length = 4096\nperformance_schema_max_sql_text_length = 4096".into()),
                cost: Some("A little more memory per digest.".into()),
            },
        );
        f.confidence = Confidence::Medium;
        out.push(f);
    }
    out
}
