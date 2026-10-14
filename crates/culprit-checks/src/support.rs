use culprit_core::{
    Capability, Category, Confidence, Engine, Finding, Impact, QueryRef, QueryStat, Severity, Snapshot, Subject,
    SubjectKind, format,
};

pub fn finding(check: &str, key: &str, category: Category, subject: Subject) -> Finding {
    Finding {
        id: format!("{check}:{key}"),
        check: check.to_string(),
        category,
        severity: Severity::Low,
        confidence: Confidence::Medium,
        title: String::new(),
        summary: String::new(),
        subject,
        impact: Impact::default(),
        metrics: Vec::new(),
        queries: Vec::new(),
        fix: None,
        verify: Vec::new(),
        caveats: Vec::new(),
        last_seen: None,
        inactive: false,
    }
}

pub fn subject(kind: SubjectKind, name: impl Into<String>) -> Subject {
    Subject { kind, name: name.into() }
}

/// Ranking score for findings measured in database time: √share keeps a 4% query above a 1% one without letting one giant query flatten everything else.
pub fn score_for_share(share: f64) -> f64 {
    (share.clamp(0.0, 1.0).sqrt() * 100.0).min(100.0)
}

pub fn severity_for_share(share: f64) -> Severity {
    if share >= 0.15 {
        return Severity::High;
    }
    if share >= 0.03 {
        return Severity::Medium;
    }
    Severity::Low
}

pub fn base_score(severity: Severity) -> f64 {
    match severity {
        Severity::High => 45.0,
        Severity::Medium => 22.0,
        Severity::Low => 8.0,
    }
}

pub struct QueryImpact {
    pub share: f64,
    /// Database time a day, when the window is long enough to say so without extrapolating.
    pub per_day_ms: Option<f64>,
}

pub fn query_impact(snapshot: &Snapshot, queries: &[&QueryStat]) -> Option<QueryImpact> {
    let workload = snapshot.workload.as_ref()?;
    if queries.is_empty() {
        return None;
    }
    let total_ms: f64 = queries.iter().map(|q| q.total_time_ms).sum();
    Some(QueryImpact { share: workload.share(total_ms), per_day_ms: workload.per_day_measured(total_ms) })
}

pub fn apply_query_impact(f: &mut Finding, impact: &QueryImpact) {
    f.impact.db_time_share = Some(impact.share);
    f.impact.time_ms_per_day = impact.per_day_ms;
    f.impact.score = score_for_share(impact.share);
    f.impact.label = share_label(impact.share);
}

pub fn share_label(share: f64) -> String {
    format!("{} of DB time", format::percent(share))
}

pub fn query_refs(queries: &[&QueryStat]) -> Vec<QueryRef> {
    queries.iter().map(|q| QueryRef { id: q.id.clone(), plan_node: None, note: None }).collect()
}

/// Queries that touch a table: by plan objects when a plan exists, otherwise by a whole-word match in the text.
pub fn queries_touching<'a>(snapshot: &'a Snapshot, schema: &str, table: &str) -> Vec<&'a QueryStat> {
    let qualified = format!("{schema}.{table}").to_ascii_lowercase();
    let table_lower = table.to_ascii_lowercase();
    snapshot
        .queries()
        .iter()
        .filter(|q| match &q.plan {
            Some(plan) => plan.nodes().iter().any(|n| {
                n.object.as_deref().is_some_and(|o| {
                    let o = o.to_ascii_lowercase().replace(['[', ']', '"'], "");
                    o == qualified || o.ends_with(&format!(".{qualified}")) || o == table_lower
                })
            }),
            None => mentions_word(&q.text, &table_lower),
        })
        .collect()
}

/// Statements reading the engine's own catalogs (monitoring tools, IDEs, migrations' introspection): real load, but not the application's to fix.
pub fn is_monitoring_query(q: &QueryStat) -> bool {
    is_monitoring_text(&q.text)
}

/// Statements that read the server's own catalog and statistics (monitoring tools, audits, Culprit-like agents), in
/// plain or digest form (MySQL digests quote names: `` `performance_schema` . `events_waits_current` ``).
pub fn is_monitoring_text(text: &str) -> bool {
    let t = text.to_ascii_lowercase().replace("` . `", ".").replace('`', "");
    [
        "sys.dm_",
        "sys.query_store",
        "query_store_",
        "sys.objects",
        "sys.columns",
        "information_schema.",
        "pg_catalog.",
        "pg_stat_",
        "performance_schema.",
        " mysql.",
        "(mysql.",
        " sys.",
        "(sys.",
    ]
    .iter()
    .any(|marker| t.contains(marker))
}

/// The application's queries in the snapshot, without monitoring and catalog statements.
pub fn app_queries(snapshot: &Snapshot) -> impl Iterator<Item = &QueryStat> {
    snapshot.queries().iter().filter(|q| !is_monitoring_query(q))
}

pub fn mentions_word(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mut start = 0;
    while let Some(pos) = lower[start..].find(word) {
        let at = start + pos;
        let before = lower[..at].chars().next_back();
        let after = lower[at + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if boundary(before) && boundary(after) {
            return true;
        }
        start = at + word.len();
    }
    false
}

pub fn quote(engine: Engine, ident: &str) -> String {
    match engine {
        Engine::Mssql => format!("[{}]", ident.replace(']', "]]")),
        Engine::Postgres => {
            let simple = ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !ident.starts_with(|c: char| c.is_ascii_digit());
            if simple { ident.to_string() } else { format!("\"{}\"", ident.replace('"', "\"\"")) }
        }
        Engine::Mysql => format!("`{}`", ident.replace('`', "``")),
    }
}

pub fn qualified(engine: Engine, schema: &str, name: &str) -> String {
    format!("{}.{}", quote(engine, schema), quote(engine, name))
}

pub fn index_name(table: &str, columns: &[String]) -> String {
    let mut name = format!("IX_{table}");
    for c in columns {
        name.push('_');
        name.push_str(&c.replace(' ', ""));
    }
    name.chars().filter(|c| c.is_alphanumeric() || *c == '_').take(60).collect()
}

pub fn supports_online_index(snapshot: &Snapshot) -> bool {
    if snapshot.server.cloud.is_some() {
        return true;
    }
    snapshot
        .server
        .edition
        .as_deref()
        .is_some_and(|e| e.contains("Enterprise") || e.contains("Developer") || e.contains("Azure"))
}

/// [`create_index_sql`] for an index that holds only the rows `filter` keeps (SQL Server, written as it keeps filters:
/// `([is_deleted]=(0))`).
pub fn create_filtered_index_sql(
    snapshot: &Snapshot,
    schema: &str,
    table: &str,
    keys: &[String],
    include: &[String],
    filter: &str,
) -> String {
    let sql = create_index_sql(snapshot, schema, table, keys, include);
    let filter = format!("\n    WHERE {filter}");
    match sql.find("\n    WITH (") {
        Some(at) => format!("{}{filter}{}", &sql[..at], &sql[at..]),
        None => format!("{}{filter};", sql.trim_end_matches(';')),
    }
}

pub fn create_index_sql(snapshot: &Snapshot, schema: &str, table: &str, keys: &[String], include: &[String]) -> String {
    let engine = snapshot.server.engine;
    let name = index_name(table, keys);
    let cols = keys.iter().map(|c| quote(engine, c)).collect::<Vec<_>>().join(", ");
    let target = qualified(engine, schema, table);
    match engine {
        Engine::Mssql => {
            let mut sql = format!("CREATE NONCLUSTERED INDEX {}\n    ON {target} ({cols})", quote(engine, &name));
            if !include.is_empty() {
                let inc = include.iter().map(|c| quote(engine, c)).collect::<Vec<_>>().join(", ");
                sql.push_str(&format!("\n    INCLUDE ({inc})"));
            }
            if supports_online_index(snapshot) {
                sql.push_str("\n    WITH (ONLINE = ON)");
            }
            sql.push(';');
            sql
        }
        Engine::Postgres => {
            let mut sql = format!(
                "CREATE INDEX CONCURRENTLY {}\n    ON {target} ({cols})",
                quote(engine, &name.to_ascii_lowercase())
            );
            if !include.is_empty() {
                let inc = include.iter().map(|c| quote(engine, c)).collect::<Vec<_>>().join(", ");
                sql.push_str(&format!("\n    INCLUDE ({inc})"));
            }
            sql.push(';');
            sql
        }
        Engine::Mysql => {
            // InnoDB has no INCLUDE: secondary indexes already carry the primary key, so extra columns would have to become keys.
            format!(
                "CREATE INDEX {}\n    ON {target} ({cols})\n    ALGORITHM=INPLACE LOCK=NONE;",
                quote(engine, &name.to_ascii_lowercase())
            )
        }
    }
}

pub fn drop_index_sql(engine: Engine, schema: &str, table: &str, index: &str) -> String {
    match engine {
        Engine::Mssql => format!("DROP INDEX {} ON {};", quote(engine, index), qualified(engine, schema, table)),
        Engine::Postgres => format!("DROP INDEX CONCURRENTLY {};", qualified(engine, schema, index)),
        // MariaDB's DROP INDEX takes no ALGORITHM/LOCK options; ALTER TABLE takes them (comma-separated) on both.
        Engine::Mysql => format!(
            "ALTER TABLE {} DROP INDEX {}, ALGORITHM=INPLACE, LOCK=NONE;",
            qualified(engine, schema, table),
            quote(engine, index)
        ),
    }
}

/// Days the cumulative counters (index usage, table stats) have been accumulating. One source for every window that
/// says so: the counters' own age as the scan measured it (on PostgreSQL from `pg_stat_database.stats_reset`, which
/// outlives a restart), else the server's start time.
pub fn counter_days(snapshot: &Snapshot) -> Option<f64> {
    if let Some(hours) = snapshot.pressure.as_ref().and_then(|p| p.uptime_hours) {
        return Some((hours / 24.0).max(0.0));
    }
    let started = snapshot.server.started_at.as_deref()?;
    let days = days_between(started, &snapshot.taken_at)?;
    Some(days.max(0.0))
}

/// Whether the counters cover enough time (20 hours) for a per-day rate that isn't an extrapolation.
pub fn per_day_is_measured(days: f64) -> bool {
    days * 24.0 >= culprit_core::PER_DAY_MIN_HOURS
}

/// "about 1.2K times a day" over 20 hours or more of counters, otherwise what was counted and over how long: "340 times
/// in the 5 hours since the server last restarted".
pub fn how_many(snapshot: &Snapshot, total: f64, days: f64, one: &str, many: &str) -> String {
    if per_day_is_measured(days) {
        let per_day = total / days;
        format!("about {} {} a day", format::count(per_day), if per_day.round() == 1.0 { one } else { many })
    } else {
        format!("{} {} {}", format::count(total), if total == 1.0 { one } else { many }, since_phrase(snapshot))
    }
}

/// Whether the counters started at a statistics reset rather than at the last restart (PostgreSQL keeps statistics
/// across a clean restart, and `pg_stat_reset()` starts them over without one).
fn counters_since_reset(snapshot: &Snapshot) -> bool {
    let (Some(hours), Some(started)) =
        (snapshot.pressure.as_ref().and_then(|p| p.uptime_hours), snapshot.server.started_at.as_deref())
    else {
        return false;
    };
    let since_start = days_between(started, &snapshot.taken_at).map(|d| d * 24.0);
    snapshot.server.engine == Engine::Postgres && since_start.is_some_and(|h| (h - hours).abs() > 0.1)
}

pub fn days_between(from: &str, to: &str) -> Option<f64> {
    let a = parse_epoch_seconds(from)?;
    let b = parse_epoch_seconds(to)?;
    Some((b - a) / 86_400.0)
}

/// Seconds since 1970 for an ISO timestamp (`2026-10-14T08:54:29Z`, with or without an offset); None for anything
/// that isn't a real date and time. The one parser is `culprit_core::format::epoch_seconds`.
pub fn parse_epoch_seconds(text: &str) -> Option<f64> {
    culprit_core::format::epoch_seconds(text)
}

pub fn since_phrase(snapshot: &Snapshot) -> String {
    // MariaDB's userstat counters run from when userstat was turned on (or flushed), which the server doesn't say.
    if snapshot.server.engine == Engine::Mysql
        && !snapshot.server.has(Capability::PerformanceSchema)
        && snapshot.server.has(Capability::IndexUsage)
    {
        return "since userstat was turned on or its statistics last flushed".to_string();
    }
    let since = if counters_since_reset(snapshot) {
        "since its statistics were last reset"
    } else {
        "since the server last restarted"
    };
    match counter_days(snapshot) {
        Some(days) if days >= 2.0 => format!("in the {} days {since}", days.round()),
        Some(days) if days * 24.0 < 1.0 => {
            let minutes = (days * 24.0 * 60.0).round().max(1.0);
            if minutes == 1.0 { format!("in the minute {since}") } else { format!("in the {minutes} minutes {since}") }
        }
        Some(days) if days * 24.0 < 1.5 => format!("in the hour {since}"),
        Some(days) => format!("in the {} hours {since}", (days * 24.0).round()),
        None => since.to_string(),
    }
}

/// "a read from Orders", "an UPDATE of Users" — a short handle for a query in titles.
pub fn query_handle(q: &QueryStat) -> String {
    if let Some(object) = &q.object_name {
        return format!("a statement in {object}");
    }
    statement_handle(&q.text)
}

/// `query_handle` for bare statement text.
pub fn statement_handle(text: &str) -> String {
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    let lower = text.to_ascii_lowercase();
    let verb = ["select", "insert", "update", "delete", "merge", "with"]
        .into_iter()
        .find(|v| lower.starts_with(v))
        .unwrap_or("");
    // A read whose FROM is a subquery or a VALUES list names its first joined table instead.
    let table = main_table(text, &lower, verb)
        .or_else(|| matches!(verb, "select" | "with").then(|| joined_table(text, &lower)).flatten());
    match (verb, table) {
        ("select" | "with", Some(t)) => format!("a read from {t}"),
        ("insert", Some(t)) => format!("an insert into {t}"),
        ("update", Some(t)) => format!("an update of {t}"),
        ("delete", Some(t)) => format!("a delete from {t}"),
        ("merge", Some(t)) => format!("a merge into {t}"),
        // A read from something else (a subquery, VALUES) is still a read, not a call to its first function.
        ("select" | "with", None) if lower.contains(" from ") => "a read".to_string(),
        // Statements without a table still have a name people recognise: `SELECT SLEEP(?)`, `DO SLEEP(?)`, `CALL p(?)`.
        _ => other_handle(text, &lower).unwrap_or_else(|| "a query".to_string()),
    }
}

/// `CALL proc(…)`, `EXEC proc`, `DO f(…)`, `SELECT f(…)` without a table: name the routine or function. Names may be
/// quoted the engine's way (MySQL's digest text writes ``DO `SLEEP` (?)``).
fn other_handle(text: &str, lower: &str) -> Option<String> {
    // The name at `from`, parts unquoted and joined by dots, and the text after it.
    let name_at = |from: usize| -> Option<(String, &str)> {
        let mut rest = text.get(from..)?.trim_start();
        let mut parts = Vec::new();
        loop {
            let close = match rest.chars().next()? {
                '`' => Some('`'),
                '[' => Some(']'),
                '"' => Some('"'),
                _ => None,
            };
            if close.is_some() {
                rest = &rest[1..];
            }
            let part: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if part.is_empty() {
                return None;
            }
            rest = &rest[part.len()..];
            if let Some(c) = close {
                rest = rest.strip_prefix(c)?;
            }
            parts.push(part);
            match rest.strip_prefix('.') {
                Some(next) => rest = next,
                None => break,
            }
        }
        Some((parts.join("."), rest))
    };
    for (verb, what) in [("call ", "a call to"), ("exec ", "a call to"), ("execute ", "a call to")] {
        if lower.starts_with(verb) {
            return name_at(verb.len()).map(|(n, _)| format!("{what} {n}"));
        }
    }
    for verb in ["do ", "select "] {
        if lower.starts_with(verb)
            && let Some((name, after)) = name_at(verb.len())
            && after.trim_start().starts_with('(')
        {
            return Some(format!("a call to {name}"));
        }
    }
    None
}

/// The first table after a JOIN, as `main_table` reads names.
fn joined_table(text: &str, lower: &str) -> Option<String> {
    let at = lower.find(" join ")? + " join ".len();
    let rest = text[at..].trim_start();
    let token: String =
        rest.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | '"' | '`')).collect();
    let name = token.rsplit('.').next()?.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`')).to_string();
    (!name.is_empty()).then_some(name)
}

/// `lower` is `text` ASCII-lowercased, so byte offsets line up and the name keeps its original casing.
fn main_table(text: &str, lower: &str, verb: &str) -> Option<String> {
    let marker = match verb {
        "insert" | "merge" => " into ",
        "update" => "update ",
        _ => " from ",
    };
    let at = lower.find(marker)? + marker.len();
    let rest = text[at..].trim_start();
    let token: String =
        rest.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | '"' | '`')).collect();
    let name = token.rsplit('.').next()?.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`')).to_string();
    (!name.is_empty() && name != "(").then_some(name)
}

/// "about 48K times a day" over a day or more of history, otherwise the real count: extrapolating 35 minutes to a day misleads.
pub fn runs_phrase(snapshot: &Snapshot, executions: f64) -> String {
    // "once", never "1 times".
    let times = |n: f64| match format::count(n).as_str() {
        "1" => "once".to_string(),
        count => format!("{count} times"),
    };
    match &snapshot.workload {
        Some(w) if w.window_hours >= 20.0 => format!("about {} a day", times(w.per_day(executions))),
        Some(w) => format!("{} in {}", times(executions), culprit_core::span_words(w.window_hours)),
        None => times(executions),
    }
}

/// Share of database time behind the top three active findings: their distinct queries, each counted once, so
/// two findings about the same query don't double it. The one number both the summary and the dashboard state.
pub fn top_findings_share<'a>(
    snapshot: &Snapshot,
    findings: impl IntoIterator<Item = &'a culprit_core::Finding>,
) -> Option<f64> {
    let w = snapshot.workload.as_ref()?;
    let ids: std::collections::BTreeSet<&str> = findings
        .into_iter()
        .filter(|f| !f.inactive)
        .take(3)
        .flat_map(|f| f.queries.iter().map(|q| q.id.as_str()))
        .collect();
    if ids.is_empty() {
        return None;
    }
    let time: f64 = w.queries.iter().filter(|q| ids.contains(q.id.as_str())).map(|q| q.total_time_ms).sum();
    Some(w.share(time).min(1.0))
}

/// "the last 7 days" for windowed history, "the 35 minutes since statistics were reset" for cumulative counters.
pub fn window_phrase(workload: &culprit_core::Workload) -> String {
    workload.window_phrase()
}

/// The window a summary names: the one the scan asked for. Query Store keeps whole intervals (an hour by default), so
/// `--window 6h` reads up to an interval more, and the rounding is said when it shows: "the last 6 hours (Query Store
/// keeps whole intervals, so this covers 7)". Data that covers less than asked for is named as it is.
pub fn summary_window(snapshot: &Snapshot) -> Option<String> {
    let w = snapshot.workload.as_ref()?;
    let asked = snapshot.options.as_ref().map(|o| f64::from(o.window_in_minutes()) / 60.0);
    let Some(asked) = asked.filter(|a| {
        w.windowed && snapshot.server.engine == Engine::Mssql && w.window_hours > *a && w.window_hours <= a + 1.5
    }) else {
        return Some(w.window_phrase());
    };
    let (said, covered) = (culprit_core::span_words(asked), culprit_core::span_words(w.window_hours));
    if said == covered {
        return Some(format!("the last {said}"));
    }
    // "7" after "6 hours"; the unit too when it differs.
    let covered = match (said.split_once(' '), covered.split_once(' ')) {
        (Some((_, unit)), Some((n, other))) if unit == other => n.to_string(),
        _ => covered,
    };
    Some(format!("the last {said} (Query Store keeps whole intervals, so this covers {covered})"))
}

/// `value` from the workload window as a daily rate for comparing with a threshold, over at least an hour of data
/// ([`culprit_core::RATE_MIN_HOURS`]); None under that, so minutes of history never pass for a busy day. Sentences that
/// say "a day" use [`culprit_core::Workload::per_day_measured`] or [`runs_phrase`] instead.
pub fn rate_per_day(snapshot: &Snapshot, value: f64) -> Option<f64> {
    snapshot.workload.as_ref()?.rate_per_day(value)
}

/// Days of cumulative counters (see [`counter_days`]) when they cover at least an hour
/// ([`culprit_core::RATE_MIN_HOURS`]): enough to compare a rate with a threshold. Sentences that say "a day" also need
/// [`per_day_is_measured`], or say what was counted with [`how_many`].
pub fn counter_days_for_rate(snapshot: &Snapshot) -> Option<f64> {
    counter_days(snapshot).filter(|d| d * 24.0 >= culprit_core::RATE_MIN_HOURS)
}

/// A cumulative counter's `total` (since the server started or statistics were reset) as a daily rate for comparing
/// with a threshold (see [`counter_days_for_rate`]).
pub fn counter_rate_per_day(snapshot: &Snapshot, total: f64) -> Option<f64> {
    counter_days_for_rate(snapshot).map(|d| total / d)
}

pub fn lower_set(items: &[String]) -> Vec<String> {
    let mut v: Vec<String> = items.iter().map(|s| s.to_ascii_lowercase()).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Names and statements come from the database in any script: every text helper slices only at character
    /// boundaries, so none of them can panic on a scan.
    #[test]
    fn text_helpers_never_panic_on_non_ascii() {
        let texts = [
            "SELECT * FROM [dbo].[Bestellungen_Übersicht] AS b JOIN İstanbul_şube s ON s.id = b.id WHERE b.ñ = N'ç' AND b.x::text LIKE 'é%'",
            "UPDATE 顧客 SET 名前 = @p1 WHERE ид = @p2",
            "INSERT INTO `tábla` (`ő`) VALUES (?)",
            "CALL `prøcédure`(?)",
            "ǅ İ ß ﬁ 𝔘 :: AS İ AND ǅ",
            "",
        ];
        for text in texts {
            let _ = mentions_word(text, "i̇stanbul_şube");
            let _ = mentions_word(text, "ß");
            let _ = predicate_columns(text);
            let _ = parse_epoch_seconds(text);
            let _ = parse_epoch_seconds("2026-10-14T08:54:29+İ");
            // Out-of-range parts from a hostile saved report: no overflow, just not a time.
            assert_eq!(parse_epoch_seconds("9223372036854775807-12-31T00:00:00Z"), None);
            assert_eq!(parse_epoch_seconds("2026-9223372036854775807-01T00:00:00Z"), None);
            assert_eq!(parse_epoch_seconds("2026-10-14Tinf:00:00Z"), None);
            assert!(parse_epoch_seconds("2026-10-14T08:54:29Z").is_some());
            let mut q = crate::waits::snapshot_with(Engine::Mssql).queries()[0].clone();
            q.text = text.into();
            let _ = query_handle(&q);
        }
        let server = crate::waits::snapshot_with(Engine::Postgres).server;
        let _ = setting_change(&server, "work_mem", "64MB ünï");
    }

    #[test]
    fn usage_windows_say_how_long_the_server_has_been_up() {
        let mut s = crate::waits::snapshot_with(Engine::Mssql);
        s.taken_at = "2026-10-11T17:05:00Z".into();
        s.pressure = None;
        let mut at = |started: &str| {
            s.server.started_at = Some(started.into());
            since_phrase(&s)
        };
        assert_eq!(at("2026-10-11T16:49:00Z"), "in the 16 minutes since the server last restarted");
        assert_eq!(at("2026-10-11T16:05:00Z"), "in the hour since the server last restarted");
        assert_eq!(at("2026-10-11T12:05:00Z"), "in the 5 hours since the server last restarted");
        assert_eq!(at("2026-10-01T17:05:00Z"), "in the 10 days since the server last restarted");
    }

    #[test]
    fn postgres_counters_that_outlived_a_restart_say_so() {
        // The server restarted 5 hours ago, but its statistics were last reset 10.9 hours ago and survived.
        let mut s = crate::waits::snapshot_with(Engine::Postgres);
        s.taken_at = "2026-10-11T17:00:00Z".into();
        s.server.started_at = Some("2026-10-11T12:00:00Z".into());
        s.pressure = Some(culprit_core::Pressure { uptime_hours: Some(10.9), ..Default::default() });
        assert_eq!(since_phrase(&s), "in the 11 hours since its statistics were last reset");
        assert!((counter_days(&s).unwrap() * 24.0 - 10.9).abs() < 1e-9);
        // Counters as old as the server: a restart, as before.
        s.pressure = Some(culprit_core::Pressure { uptime_hours: Some(5.0), ..Default::default() });
        assert_eq!(since_phrase(&s), "in the 5 hours since the server last restarted");
    }

    #[test]
    fn counts_are_per_day_only_over_a_day_of_counters() {
        let mut s = crate::waits::snapshot_with(Engine::Mssql);
        s.taken_at = "2026-10-11T17:05:00Z".into();
        s.pressure = None;
        s.server.started_at = Some("2026-10-11T12:05:00Z".into());
        let days = counter_days(&s).unwrap();
        // 5 hours: what was counted, not "~1.6K times a day".
        assert_eq!(
            how_many(&s, 340.0, days, "time", "times"),
            "340 times in the 5 hours since the server last restarted"
        );
        s.server.started_at = Some("2026-10-09T17:05:00Z".into());
        let days = counter_days(&s).unwrap();
        assert_eq!(how_many(&s, 340.0, days, "time", "times"), "about 170 times a day");
    }

    #[test]
    fn system_schema_reads_are_monitoring_on_every_engine() {
        assert!(is_monitoring_text(
            "SELECT `EVENT_NAME` FROM `performance_schema` . `events_waits_summary_global_by_event_name`"
        ));
        assert!(is_monitoring_text("SELECT * FROM mysql.slow_log WHERE db = ?"));
        assert!(is_monitoring_text("SELECT * FROM sys.innodb_lock_waits"));
        assert!(is_monitoring_text("SELECT * FROM `information_schema` . `TABLES`"));
        assert!(!is_monitoring_text("SELECT `id` FROM `orders` WHERE `customer_id` = ?"));
        assert!(!is_monitoring_text("SELECT analysis.id FROM analysis WHERE mysqlish = 1"));
    }

    #[test]
    fn epoch_parsing_handles_offsets() {
        let a = parse_epoch_seconds("2026-10-01T00:00:00Z").unwrap();
        let b = parse_epoch_seconds("2026-10-01T02:00:00+02:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(days_between("2026-10-01T00:00:00Z", "2026-10-11 12:00:00").unwrap(), 10.5);
    }

    #[test]
    fn word_matching() {
        assert!(mentions_word("SELECT * FROM [dbo].[Orders] m", "orders"));
        assert!(!mentions_word("SELECT * FROM OrdersArchive", "orders"));
    }

    #[test]
    fn query_handles() {
        let mut q = sample();
        q.text = "SELECT TOP 10 * FROM [dbo].[Orders] WHERE x = @p1".into();
        assert_eq!(query_handle(&q), "a read from Orders");
        q.text = "UPDATE dbo.Users SET a = 1".into();
        assert_eq!(query_handle(&q), "an update of Users");
    }

    pub(crate) fn sample() -> QueryStat {
        QueryStat {
            id: "1".into(),
            query_hash: None,
            text: String::new(),
            parameters: None,
            object_name: None,
            executions: 1.0,
            total_time_ms: 1.0,
            mean_time_ms: 1.0,
            max_time_ms: None,
            stddev_time_ms: None,
            cpu_time_ms: None,
            rows: None,
            logical_reads: None,
            physical_reads: None,
            temp_writes: None,
            first_seen: None,
            last_seen: None,
            plans: Vec::new(),
            plan: None,
            waits: Vec::new(),
            trend: None,
            spread: None,
            costs: None,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct PredicateColumns {
    pub equality: Vec<String>,
    pub range: Vec<String>,
}

const OPERATORS: [(&str, bool); 11] = [
    (" = ANY ", true),
    (" IN ", true),
    (" LIKE ", false),
    ("<>", false),
    ("!=", false),
    (">=", false),
    ("<=", false),
    ("~~", false),
    ("=", true),
    ("<", false),
    (">", false),
];

/// Columns compared against a parameter or literal in a plan predicate, e.g. `(customer_id = $1)` or
/// `[db].[dbo].[Orders].[CustomerId] as [m].[CustomerId]=[@P1]`. Column-to-column comparisons (joins) are skipped.
pub fn predicate_columns(predicate: &str) -> PredicateColumns {
    let cleaned = strip_casts(&predicate.replace(['[', ']', '"', '(', ')'], ""));
    let mut out = PredicateColumns::default();
    for conjunct in split_ci(&cleaned, " AND ") {
        let upper = conjunct.to_ascii_uppercase();
        let Some((at, op, equality)) = OPERATORS
            .iter()
            .filter_map(|(op, eq)| upper.find(op).map(|at| (at, *op, *eq)))
            .min_by_key(|(at, _, _)| *at)
        else {
            continue;
        };
        let left = drop_alias(conjunct[..at].trim());
        let right = drop_alias(conjunct[at + op.len()..].trim());
        let column = match (column_name(left), column_name(right)) {
            (Some(c), None) if is_value(right) => c,
            (None, Some(c)) if is_value(left) => c,
            _ => continue,
        };
        let list = if equality { &mut out.equality } else { &mut out.range };
        if !list.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
            list.push(column);
        }
    }
    out.range.retain(|r| !out.equality.iter().any(|e| e.eq_ignore_ascii_case(r)));
    out
}

fn split_ci<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let upper = text.to_ascii_uppercase();
    let mut parts = Vec::new();
    let mut start = 0;
    while let Some(pos) = upper[start..].find(separator) {
        parts.push(&text[start..start + pos]);
        start += pos + separator.len();
    }
    parts.push(&text[start..]);
    parts
}

fn strip_casts(text: &str) -> String {
    const MULTIWORD: [&str; 4] = [" with time zone", " without time zone", " varying", " precision"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("::") {
        out.push_str(&rest[..pos]);
        rest = rest[pos + 2..].trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
        while let Some(suffix) = MULTIWORD.iter().find(|m| rest.to_ascii_lowercase().starts_with(*m)) {
            rest = &rest[suffix.len()..];
        }
        if let Some(stripped) = rest.strip_prefix("[]") {
            rest = stripped;
        }
    }
    out.push_str(rest);
    out
}

fn drop_alias(side: &str) -> &str {
    let upper = side.to_ascii_uppercase();
    match upper.find(" AS ") {
        Some(pos) => side[..pos].trim(),
        None => side,
    }
}

fn column_name(side: &str) -> Option<String> {
    let side = side.trim();
    if side.is_empty() || is_value(side) || side.contains(' ') {
        return None;
    }
    let last = side.rsplit('.').next()?.trim();
    let valid = last.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && last.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    (valid && !last.contains("CONVERT_IMPLICIT")).then(|| last.to_string())
}

fn is_value(side: &str) -> bool {
    let s = side.trim();
    let upper = s.to_ascii_uppercase();
    s.starts_with(['$', '@', '\'', '-', '?'])
        || s.chars().next().is_some_and(|c| c.is_ascii_digit())
        || upper.starts_with("NULL")
        || upper.starts_with("N'")
        || upper.starts_with("ARRAY")
        || upper.starts_with("{")
        || upper.starts_with("CONVERT_IMPLICIT") && (upper.contains("@") || upper.contains("$"))
        || upper.starts_with("NOW")
        || upper.starts_with("CURRENT_")
        || upper.starts_with("GETDATE")
        || upper.starts_with("SYSUTCDATETIME")
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    #[test]
    fn statements_without_a_table_are_named() {
        assert_eq!(statement_handle("DO SLEEP(?)"), "a call to SLEEP");
        assert_eq!(statement_handle("SELECT sleep(?)"), "a call to sleep");
        assert_eq!(statement_handle("SELECT pg_sleep($1)"), "a call to pg_sleep");
        assert_eq!(statement_handle("DO `SLEEP` (?)"), "a call to SLEEP");
        assert_eq!(statement_handle("CALL `refresh_totals` (?)"), "a call to refresh_totals");
        assert_eq!(statement_handle("EXEC [dbo].[usp_orders] @id = 1"), "a call to dbo.usp_orders");
        assert_eq!(statement_handle("CALL shop.refresh(?)"), "a call to shop.refresh");
        assert_eq!(statement_handle("CALL refresh_totals(?)"), "a call to refresh_totals");
        assert_eq!(statement_handle("SELECT id FROM orders WHERE id = ?"), "a read from orders");
        assert_eq!(statement_handle("SELECT 1"), "a query");
        assert_eq!(
            statement_handle("SELECT count(*) FROM (VALUES ($1),($2)) v(x) JOIN orders o ON o.id = v.x"),
            "a read from orders"
        );
        assert_eq!(statement_handle("SELECT count(*) FROM (SELECT 1) s"), "a read");
    }

    #[test]
    fn mysql_online_ddl_options_are_space_separated() {
        // `ALGORITHM=INPLACE, LOCK=NONE` is ERROR 1064 in CREATE/DROP INDEX on MySQL and MariaDB: index options don't
        // take commas (only ALTER TABLE's clauses do).
        // MariaDB's DROP INDEX has no such options at all, so the drop goes through ALTER TABLE (whose clauses do).
        let drop = drop_index_sql(Engine::Mysql, "shop", "orders", "ix_orders_status");
        assert_eq!(drop, "ALTER TABLE `shop`.`orders` DROP INDEX `ix_orders_status`, ALGORITHM=INPLACE, LOCK=NONE;");
    }

    #[test]
    fn postgres_predicates() {
        let p = predicate_columns("(customer_id = $1)");
        assert_eq!(p.equality, vec!["customer_id"]);
        let p = predicate_columns("(((status)::text = 'open'::text) AND (created_at > (now() - '7 days'::interval)))");
        assert_eq!(p.equality, vec!["status"]);
        assert_eq!(p.range, vec!["created_at"]);
        let p = predicate_columns("(o.customer_id = c.id)");
        assert!(p.equality.is_empty());
    }

    #[test]
    fn sql_server_predicates() {
        let p = predicate_columns(
            "[shop].[dbo].[Orders].[CustomerId] as [m].[CustomerId]=[@P1] AND [shop].[dbo].[Orders].[CreatedAt] as [m].[CreatedAt]>=[@P2]",
        );
        assert_eq!(p.equality, vec!["CustomerId"]);
        assert_eq!(p.range, vec!["CreatedAt"]);
        let p = predicate_columns("[shop].[dbo].[Users].[TenantId]=CONVERT_IMPLICIT(uniqueidentifier,[@P1],0)");
        assert_eq!(p.equality, vec!["TenantId"]);
    }
}

/// How to change a server setting where this server runs. On a server you manage it is SQL (`ALTER SYSTEM` and a
/// reload on PostgreSQL, `SET PERSIST`/`SET GLOBAL` on MySQL and MariaDB); a managed service refuses those, so there it
/// is the platform's own control (a DB parameter group, server parameters, database flags) and no SQL. Every check
/// that suggests a setting words it with this, so advice is always something the reader can run.
pub struct SettingChange {
    /// One sentence: where and how to set it.
    pub how: String,
    /// The statements, on servers where they work.
    pub sql: Option<String>,
}

pub fn setting_change(server: &culprit_core::ServerInfo, name: &str, value: &str) -> SettingChange {
    let managed = |control: &str| SettingChange { how: format!("Set {name} to {value} in {control}."), sql: None };
    match server.cloud.as_deref() {
        Some("Amazon RDS") => return managed("the instance's DB parameter group"),
        Some("Amazon Aurora") => return managed("the cluster's (or instance's) DB parameter group"),
        Some(c) if c.starts_with("Azure Database for") => {
            return managed("the server parameters (Azure portal or `az … server parameter set`)");
        }
        Some("Google Cloud SQL") => {
            return managed(
                "the instance's database flags (Cloud SQL console or `gcloud sql instances patch --database-flags`)",
            );
        }
        _ => {}
    }
    let quoted =
        |v: &str| if v.chars().all(|c| c.is_ascii_digit() || c == '.') { v.to_string() } else { format!("'{v}'") };
    match server.engine {
        Engine::Postgres => SettingChange {
            how: format!(
                "Set {name} to {value} and reload the configuration (settings marked as needing a restart take effect after one)."
            ),
            sql: Some(format!("ALTER SYSTEM SET {name} = {};\nSELECT pg_reload_conf();", quoted(value))),
        },
        Engine::Mysql if server.name() == "MySQL" && culprit_core::version_at_least(&server.version, [8, 0, 0]) => {
            SettingChange {
                how: format!("Set {name} to {value}; SET PERSIST keeps it across restarts."),
                sql: Some(format!("SET PERSIST {name} = {};", quoted(value))),
            }
        }
        Engine::Mysql => SettingChange {
            how: format!(
                "Set {name} to {value} now, and in the server's option file (my.cnf) so it survives a restart."
            ),
            sql: Some(format!("SET GLOBAL {name} = {};", quoted(value))),
        },
        Engine::Mssql => SettingChange {
            how: format!("Set {name} to {value}."),
            sql: Some(format!("EXEC sp_configure '{name}', {};\nRECONFIGURE;", quoted(value))),
        },
    }
}
