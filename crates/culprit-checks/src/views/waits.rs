//! Waits, explained: each category of waiting in plain words, what the numbers say about this database, and the queries
//! behind it. Pure; the same report for SQL Server, PostgreSQL and MySQL, honest about what each engine measures.

use std::collections::BTreeMap;

use culprit_core::waits::{category_info, category_key};
use culprit_core::{Capability, Engine, QueryStat, Snapshot, WaitCategory, WaitQuery, WaitScope, WaitsReport, format};

use crate::support::{app_queries, setting_change};

/// Queries listed per category.
const MAX_QUERIES: usize = 12;
/// "Big result sets" for the network explanation.
const BIG_RESULT_ROWS: f64 = 10_000.0;

/// Explain where the scan's database time went. `None` when the snapshot holds no waits at all.
pub fn explain(snapshot: &Snapshot) -> Option<WaitsReport> {
    let engine = snapshot.server.engine;
    let (scope, source) = scope_and_source(snapshot);
    let mut totals: BTreeMap<&'static str, (f64, BTreeMap<String, f64>)> = BTreeMap::new();
    for w in &snapshot.waits {
        let key = category_key(&w.category);
        let entry = totals.entry(key).or_default();
        entry.0 += w.wait_ms;
        if let Some(t) = &w.wait_type {
            *entry.1.entry(t.clone()).or_default() += w.wait_ms;
        }
    }
    let total_ms: f64 = totals.values().map(|(ms, _)| ms).sum();
    // Plans Culprit (or a person) asked for are not part of the application's workload.
    let queries: Vec<&QueryStat> =
        app_queries(snapshot).filter(|q| !q.text.trim_start().to_ascii_uppercase().starts_with("EXPLAIN")).collect();
    let attributed: BTreeMap<&'static str, Vec<(&QueryStat, f64)>> = attribute(&queries);
    if total_ms <= 0.0 && attributed.is_empty() {
        return None;
    }

    let mut categories: Vec<WaitCategory> = totals
        .iter()
        .filter(|(_, (ms, _))| *ms > 0.0)
        .map(|(key, (ms, types))| {
            let (label, meaning) = category_info(key, engine);
            let top_wait = types.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(t, _)| t.clone());
            let contributors = attributed.get(key).map(Vec::as_slice).unwrap_or(&[]);
            let mut category = WaitCategory {
                key: key.to_string(),
                label: label.to_string(),
                wait_ms: *ms,
                share: if total_ms > 0.0 { ms / total_ms } else { 0.0 },
                top_wait,
                meaning,
                detail: None,
                queries: Vec::new(),
                attribution: None,
            };
            fill_queries(&mut category, contributors);
            category.detail = detail(snapshot, &category, contributors);
            if category.queries.is_empty() {
                category.attribution = Some(engine.spec().wait_not_per_statement(&snapshot.server, key));
            }
            category
        })
        .collect();
    categories.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));

    let mut notes = Vec::new();
    match engine {
        Engine::Postgres => {
            notes.push("PostgreSQL keeps no history of what sessions waited on. These totals come from per-statement timing; use Right now to sample what sessions wait on at this moment.".into());
            // The setting says whether disk time is measured; a cached database reads nothing from disk with it on.
            // Without the setting in the scan, no disk time at all is the hint.
            let timing_off = match snapshot.setting("track_io_timing") {
                Some(s) => !matches!(s.value.trim().to_ascii_lowercase().as_str(), "on" | "true" | "1"),
                None => !snapshot.waits.iter().any(|w| category_key(&w.category) == "io"),
            };
            if timing_off {
                let change = setting_change(&snapshot.server, "track_io_timing", "on");
                notes.push(format!(
                    "Turn on track_io_timing so time spent reading from disk is measured per statement. {}{}",
                    change.how,
                    change.sql.map(|sql| format!(" ({})", sql.replace('\n', " "))).unwrap_or_default()
                ));
            }
        }
        Engine::Mysql if !snapshot.server.has(Capability::PerformanceSchema) => {
            notes.push(format!("performance_schema is off, so {} times no waits; the slow query log keeps each statement's lock time. Right now samples what sessions are doing.", snapshot.server.name()));
        }
        Engine::Mysql => {
            notes.push(format!("{} counts waits since it started, only for enabled performance_schema instruments. Lock time is attributed to statements; the rest is a server-wide total.", snapshot.server.name()));
        }
        Engine::Mssql if scope == WaitScope::SinceStartup => notes.push(waits_capture_note(snapshot)),
        Engine::Mssql | Engine::Sqlite => {}
    }
    Some(WaitsReport { engine, scope, source, total_ms, categories, notes })
}

/// Why SQL Server's waits aren't tied to queries, and what would tie them. Query Store captures waits per query from
/// SQL Server 2017 (and on Azure SQL); it has to be on for that, and on 2016 it can't.
fn waits_capture_note(snapshot: &Snapshot) -> String {
    const TOTALS: &str = "These are server totals since startup and can't be tied to queries.";
    let state = snapshot.setting("query_store_options.actual_state_desc").map(|s| s.value.to_ascii_uppercase());
    let on = state.as_deref().is_some_and(|s| s == "READ_WRITE" || s == "READ_ONLY");
    let azure = snapshot.server.platform().is_some_and(|p| p.is_azure_sql());
    let major: u32 = snapshot.server.version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    if !azure && major > 0 && major < 14 {
        return format!(
            "{TOTALS} Query Store on {} doesn't record waits per query; SQL Server 2017 and later do.",
            snapshot.server.product
        );
    }
    if !on {
        return format!(
            "{TOTALS} Query Store is off; with it on (and recording waits, the default from SQL Server 2017), Culprit can tie waits to queries: ALTER DATABASE CURRENT SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE, WAIT_STATS_CAPTURE_MODE = ON);"
        );
    }
    let capturing = snapshot
        .setting("query_store_options.wait_stats_capture_mode_desc")
        .is_some_and(|s| s.value.eq_ignore_ascii_case("ON"));
    if capturing {
        return format!("Query Store recorded no waits for this database's queries in the window. {TOTALS}");
    }
    format!(
        "Query Store isn't capturing waits. {TOTALS} Turn it on: ALTER DATABASE CURRENT SET QUERY_STORE (WAIT_STATS_CAPTURE_MODE = ON);"
    )
}

fn scope_and_source(snapshot: &Snapshot) -> (WaitScope, String) {
    let engine = snapshot.server.engine;
    let window = snapshot.workload.as_ref().map(|w| span(w.measurement_hours())).unwrap_or_default();
    let uptime = snapshot.pressure.as_ref().and_then(|p| p.uptime_hours).map(span);
    let attributed = app_queries(snapshot).any(|q| q.waits.iter().any(|w| w.wait_ms > 0.0));
    match engine {
        Engine::Sqlite => (WaitScope::SinceStartup, "SQLite keeps no waits".into()),
        Engine::Mssql if attributed => (WaitScope::Window, format!("Query Store, last {window}")),
        Engine::Mssql => (
            WaitScope::SinceStartup,
            uptime
                .map(|u| format!("Server totals since SQL Server started {u} ago"))
                .unwrap_or_else(|| "Server totals since SQL Server started".into()),
        ),
        Engine::Postgres => (
            WaitScope::Statements,
            if window.is_empty() {
                "Statement timing from pg_stat_statements".into()
            } else {
                format!("Statement timing from pg_stat_statements, the {window} since statistics were reset")
            },
        ),
        Engine::Mysql if !snapshot.server.has(Capability::PerformanceSchema) => {
            (WaitScope::Statements, format!("Lock time per statement from the slow query log, the last {window}"))
        }
        Engine::Mysql => {
            let name = snapshot.server.name();
            (
                WaitScope::SinceStartup,
                uptime
                    .map(|u| format!("performance_schema totals since {name} started {u} ago"))
                    .unwrap_or_else(|| format!("performance_schema totals since {name} started")),
            )
        }
    }
}

/// Each query's wait time per shared category key.
fn attribute<'a>(queries: &[&'a QueryStat]) -> BTreeMap<&'static str, Vec<(&'a QueryStat, f64)>> {
    let mut out: BTreeMap<&'static str, Vec<(&QueryStat, f64)>> = BTreeMap::new();
    for q in queries {
        let mut per_key: BTreeMap<&'static str, f64> = BTreeMap::new();
        for w in &q.waits {
            *per_key.entry(category_key(&w.category)).or_default() += w.wait_ms;
        }
        for (key, ms) in per_key {
            if ms > 0.0 {
                out.entry(key).or_default().push((*q, ms));
            }
        }
    }
    for list in out.values_mut() {
        list.sort_by(|a, b| b.1.total_cmp(&a.1));
    }
    out
}

fn fill_queries(category: &mut WaitCategory, contributors: &[(&QueryStat, f64)]) {
    let attributed: f64 = contributors.iter().map(|(_, ms)| ms).sum();
    // Shares are of the category's attributed time; Query Store attributes everything, MySQL only some categories.
    let denominator = category.wait_ms.max(attributed);
    // Contributors under half a percent are noise next to the ones that matter; keep at least the biggest.
    category.queries = contributors
        .iter()
        .enumerate()
        .filter(|(i, (_, ms))| *i == 0 || denominator <= 0.0 || ms / denominator >= 0.005)
        .map(|(_, c)| c)
        .take(MAX_QUERIES)
        .map(|(q, ms)| WaitQuery {
            id: q.id.clone(),
            text: one_line(&q.text, 160),
            wait_ms: *ms,
            share_of_category: if denominator > 0.0 { (ms / denominator).clamp(0.0, 1.0) } else { 0.0 },
            share_of_query: if q.total_time_ms > 0.0 { (ms / q.total_time_ms).clamp(0.0, 1.0) } else { 0.0 },
            executions: q.executions,
            rows_per_run: q.rows.filter(|_| q.executions > 0.0).map(|r| r / q.executions),
        })
        .collect();
}

/// One sentence about what this database's numbers say for the category.
fn detail(snapshot: &Snapshot, category: &WaitCategory, contributors: &[(&QueryStat, f64)]) -> Option<String> {
    let top = category.queries.first();
    let lead = |q: &WaitQuery| {
        format!("{} accounts for {} of it", capitalize(&handle(snapshot, &q.id)), format::percent(q.share_of_category))
    };
    match category.key.as_str() {
        "network" => {
            let big: Vec<f64> =
                category.queries.iter().filter_map(|q| q.rows_per_run).filter(|r| *r >= BIG_RESULT_ROWS).collect();
            match (big.len(), top) {
                (0, Some(q)) => Some(format!(
                    "{}; it returns {} rows per call, so look at how the application reads them.",
                    lead(q),
                    q.rows_per_run.map(format::count).unwrap_or_else(|| "?".into())
                )),
                (n, _) if n > 0 => {
                    let smallest = big.iter().cloned().fold(f64::INFINITY, f64::min);
                    Some(format!(
                        "{} {} return{} {}+ rows per call: page or filter them, or stream them without per-row work in the application.",
                        n,
                        if n == 1 { "query" } else { "queries" },
                        if n == 1 { "s" } else { "" },
                        format::count(smallest)
                    ))
                }
                _ => None,
            }
        }
        "lock" => top.map(|q| format!("{}. Live shows who blocks whom right now.", lead(q))),
        "memory" => {
            let grant = top.and_then(|q| {
                let hash = contributors.iter().find(|(s, _)| s.id == q.id).and_then(|(s, _)| s.query_hash.clone())?;
                snapshot.pressure.as_ref()?.grants.iter().find(|g| g.query_hash.eq_ignore_ascii_case(&hash)).cloned()
            });
            match (top, grant) {
                (Some(q), Some(g)) => Some(format!(
                    "{}; it reserves {} per run and uses {}.",
                    lead(q),
                    format::bytes(g.avg_granted_kb * 1024.0),
                    format::bytes(g.avg_used_kb * 1024.0)
                )),
                (Some(q), None) => Some(format!("{}.", lead(q))),
                _ => pending_grants(snapshot),
            }
        }
        "io" => top.map(|q| {
            let reads = contributors
                .iter()
                .find(|(s, _)| s.id == q.id)
                .and_then(|(s, _)| (s.executions > 0.0).then(|| s.physical_reads.map(|p| p / s.executions)).flatten());
            match reads {
                Some(r) if r >= 1.0 => format!("{}, reading {} pages from disk per run.", lead(q), format::count(r)),
                _ => format!("{}.", lead(q)),
            }
        }),
        "parallelism" => top.map(|q| format!("{}. Check its plan for a large scan an index would avoid.", lead(q))),
        "compilation" => match top {
            Some(q) => Some(format!("{}.", lead(q))),
            None => Some("Compiling is a sign of one-off statements; see the compile findings.".into()),
        },
        _ => top.map(|q| format!("{}.", lead(q))),
    }
}

fn pending_grants(snapshot: &Snapshot) -> Option<String> {
    let pending = snapshot.pressure.as_ref()?.signal("memory-grants-pending")?.value;
    (pending > 0.0).then(|| format!("{} queries were waiting for memory when the scan ran.", format::count(pending)))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

fn handle(snapshot: &Snapshot, id: &str) -> String {
    snapshot
        .workload
        .as_ref()
        .and_then(|w| w.query(id))
        .map(crate::support::query_handle)
        .unwrap_or_else(|| format!("query {id}"))
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

fn span(hours: f64) -> String {
    if hours >= 47.0 {
        return format!("{} days", (hours / 24.0).round());
    }
    if hours >= 1.5 {
        return format!("{} hours", hours.round());
    }
    let minutes = (hours * 60.0).round().max(1.0);
    if minutes == 1.0 { "1 minute".into() } else { format!("{minutes} minutes") }
}

/// Fixture snapshots for tests, one per engine.
#[cfg(test)]
pub(crate) fn snapshot_with(engine: Engine) -> Snapshot {
    let text = match engine {
        Engine::Mssql => include_str!("../../../../fixtures/mssql-demo.json"),
        Engine::Postgres => include_str!("../../../../fixtures/postgres-demo.json"),
        Engine::Mysql => include_str!("../../../../fixtures/mysql-demo.json"),
        Engine::Sqlite => panic!("no SQLite snapshot fixture for waits"),
    };
    let mut snapshot: Snapshot = serde_json::from_str(text).expect("fixture is a snapshot");
    if let Some(w) = snapshot.workload.as_mut() {
        w.queries.iter_mut().for_each(|q| q.waits.clear());
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::WaitStat;

    fn wait(category: &str, wait_type: &str, ms: f64) -> WaitStat {
        WaitStat { category: category.into(), wait_type: Some(wait_type.into()), wait_ms: ms, tasks: None }
    }

    #[test]
    fn zero_statement_waits_do_not_relabel_server_totals_as_a_query_store_window() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        snapshot.waits = vec![wait("Memory", "RESOURCE_SEMAPHORE", 600_000.0)];
        snapshot.workload.as_mut().unwrap().queries[0].waits = vec![wait("Lock", "Lock", 0.0)];
        let report = explain(&snapshot).unwrap();
        assert_eq!(report.scope, WaitScope::SinceStartup);
        assert!(report.source.starts_with("Server totals"));
    }

    #[test]
    fn categories_carry_queries_and_plain_detail() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        snapshot.waits =
            vec![wait("Network", "Network IO", 600.0), wait("Lock", "Lock", 300.0), wait("CPU", "CPU", 100.0)];
        let w = snapshot.workload.as_mut().unwrap();
        w.queries[0].rows = Some(w.queries[0].executions * 80_000.0);
        w.queries[0].waits = vec![wait("Network", "Network IO", 500.0)];
        w.queries[1].waits = vec![wait("Lock", "Lock", 300.0), wait("Network", "Network IO", 100.0)];
        let report = explain(&snapshot).unwrap();
        assert_eq!(report.scope, WaitScope::Window);
        assert_eq!(report.categories[0].key, "network");
        assert_eq!(report.categories[0].label, "Sending results");
        assert_eq!(report.categories[0].queries.len(), 2);
        assert!((report.categories[0].queries[0].share_of_category - 500.0 / 600.0).abs() < 1e-9);
        assert!(
            report.categories[0].detail.as_deref().unwrap().contains("80K+ rows per call"),
            "{:?}",
            report.categories[0].detail
        );
        assert!(report.categories[1].detail.as_deref().unwrap().contains("Live shows who blocks whom"));
        assert!(report.categories[2].attribution.is_some(), "CPU has no attributed queries");
    }

    #[test]
    fn mysql_and_postgres_say_what_they_cannot_measure() {
        let mut mysql = snapshot_with(Engine::Mysql);
        mysql.waits = vec![
            wait("Table IO", "wait/io/table/sql/handler", 900.0),
            wait("Lock", "wait/lock/table/sql/handler", 100.0),
        ];
        mysql.workload.as_mut().unwrap().queries[0].waits = vec![wait("Lock", "Lock", 80.0)];
        let report = explain(&mysql).unwrap();
        assert_eq!(report.scope, WaitScope::SinceStartup);
        let rows = report.categories.iter().find(|c| c.key == "rows").unwrap();
        assert!(rows.queries.is_empty() && rows.attribution.as_deref().unwrap().contains("server-wide"));
        let lock = report.categories.iter().find(|c| c.key == "lock").unwrap();
        assert_eq!(lock.queries.len(), 1);

        let mut pg = snapshot_with(Engine::Postgres);
        pg.waits = vec![wait("Other", "statement time", 1000.0)];
        pg.settings.retain(|s| s.name != "track_io_timing");
        let report = explain(&pg).unwrap();
        assert_eq!(report.scope, WaitScope::Statements);
        assert!(report.notes.iter().any(|n| n.contains("track_io_timing")));
    }

    // C19: the note follows the setting. A cached database with timing on reads nothing from disk, which is no reason
    // to ask for timing; with it off, the advice is worded where the server runs.
    #[test]
    fn the_io_timing_note_follows_the_setting() {
        let mut pg = snapshot_with(Engine::Postgres);
        pg.waits = vec![wait("Other", "statement time", 1000.0)];
        assert!(!explain(&pg).unwrap().notes.iter().any(|n| n.contains("track_io_timing")), "it is on in the demo");
        for s in pg.settings.iter_mut().filter(|s| s.name == "track_io_timing") {
            s.value = "off".into();
        }
        pg.server.cloud = Some("Amazon RDS".into());
        let notes = explain(&pg).unwrap().notes;
        let note = notes.iter().find(|n| n.contains("track_io_timing")).unwrap();
        assert!(note.contains("DB parameter group") && !note.contains("ALTER SYSTEM"), "{note}");
    }

    // C19: wait capture is suggested only where it exists: Query Store on, SQL Server 2017 or later.
    #[test]
    fn wait_capture_is_suggested_only_where_it_exists() {
        let mut s = snapshot_with(Engine::Mssql);
        s.waits = vec![wait("CPU", "SOS_SCHEDULER_YIELD", 100.0)];
        let set = |s: &mut Snapshot, name: &str, value: &str| {
            for x in s.settings.iter_mut().filter(|x| x.name == name) {
                x.value = value.into();
            }
        };
        set(&mut s, "query_store_options.wait_stats_capture_mode_desc", "OFF");
        let note = |s: &Snapshot| explain(s).unwrap().notes.join(" ");
        assert!(note(&s).contains("WAIT_STATS_CAPTURE_MODE = ON"));
        set(&mut s, "query_store_options.actual_state_desc", "OFF");
        assert!(note(&s).contains("Query Store is off"), "{}", note(&s));
        s.server.version = "13.0.6300.2".into();
        s.server.product = "SQL Server 2016".into();
        let text = note(&s);
        assert!(
            text.contains("SQL Server 2016 doesn't record waits per query")
                && !text.contains("WAIT_STATS_CAPTURE_MODE"),
            "{text}"
        );
    }

    // C19: SQL Server's Memory category holds allocations as well as grants, and THREADPOOL is folded into CPU.
    #[test]
    fn category_labels_say_what_they_hold() {
        let (label, meaning) = category_info("memory", Engine::Mssql);
        assert_eq!(label, "Memory");
        assert!(meaning.contains("RESOURCE_SEMAPHORE") && meaning.contains("CMEMTHREAD"));
        assert!(category_info("cpu", Engine::Mssql).1.contains("THREADPOOL"));
        assert!(!category_info("cpu", Engine::Postgres).1.contains("THREADPOOL"));
    }

    #[test]
    fn nothing_to_explain_is_none() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        snapshot.waits.clear();
        assert!(explain(&snapshot).is_none());
    }
}
