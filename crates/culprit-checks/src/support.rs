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

/// Database time a day a finding needs behind it for Medium, and for High: a share of a quiet database's time is still a
/// small number.
pub const MEDIUM_MS_A_DAY: f64 = 60_000.0;
pub const HIGH_MS_A_DAY: f64 = 600_000.0;

/// Severity for a finding from the database time behind it (`time_ms` in the workload's window, `share` of all
/// of it): the share says how much of the workload it is, the amount whether that's worth anyone's time. The amount is
/// the time a day over at least an hour of window; under an hour, the window's own total, which a day holds at least.
/// A window of unknown length (statistics never reset where the engine doesn't date them) says no amount a day: such
/// time is never High.
/// Where the source left time unattributed (MySQL's catch-all digest row once its table is full), shares are of the time
/// it could attribute: the grade takes the share of everything there may be, the lower bound.
pub fn severity_for_time(snapshot: &Snapshot, time_ms: f64, share: f64) -> Severity {
    severity_on_amount(snapshot, rate_per_day(snapshot, time_ms), time_ms, share)
}

/// [`severity_for_time`] for time spent by statements (`time_ms` of `queries`' time: theirs, their spills, their
/// waits), graded at the pace their runs came ([`rate_per_day_of`]): a burst on its total, never extrapolated to a day.
pub fn severity_for_time_of(snapshot: &Snapshot, queries: &[&QueryStat], time_ms: f64, share: f64) -> Severity {
    severity_on_amount(snapshot, rate_per_day_of(snapshot, queries, time_ms), time_ms, share)
}

fn severity_on_amount(snapshot: &Snapshot, a_day: Option<f64>, time_ms: f64, share: f64) -> Severity {
    let by_amount = match snapshot.workload.as_ref() {
        Some(w) if w.window_unknown() => Severity::Medium,
        Some(_) => severity_for_amount(a_day.unwrap_or(time_ms)),
        None => Severity::Low,
    };
    let share = match snapshot.workload.as_ref().and_then(|w| Some((w, w.untracked_time_ms?))) {
        Some((w, untracked)) if w.total_time_ms + untracked > 0.0 => share.min(time_ms / (w.total_time_ms + untracked)),
        _ => share,
    };
    severity_for_share(share).min(by_amount)
}

/// [`severity_for_time_of`] for a share of all the workload's database time spent by `queries`.
pub fn severity_for_share_of(snapshot: &Snapshot, queries: &[&QueryStat], share: f64) -> Severity {
    let total = snapshot.workload.as_ref().map_or(0.0, |w| w.total_time_ms);
    severity_for_time_of(snapshot, queries, share * total, share)
}

/// The grade of `queries`' whole impact ([`query_impact`]): over Culprit's scans where they measured them, the share
/// and the amount a day it is of what the database ran in those scans ([`severity_over_scans`]); otherwise their share
/// of the workload at the pace their runs came ([`severity_for_share_of`]).
pub fn severity_for_impact(snapshot: &Snapshot, queries: &[&QueryStat], impact: &QueryImpact) -> Severity {
    severity_for_statement_share(snapshot, queries, impact.share)
}

/// The grade of `share` of database time spent by `queries` (their share as [`query_impact`] states it, or a fraction
/// of it: what an index would save of their time), on one basis: over Culprit's scans where they measured them, the
/// share and the amount a day it is of what the database ran in those scans ([`severity_over_scans`]); otherwise at the
/// pace their runs came ([`severity_for_share_of`]). Never the scans' share with the time their totals hold.
pub fn severity_for_statement_share(snapshot: &Snapshot, queries: &[&QueryStat], share: f64) -> Severity {
    severity_over_scans(snapshot, queries, share).unwrap_or_else(|| severity_for_share_of(snapshot, queries, share))
}

/// The amount alone of [`severity_for_statement_share`]: what `share` of database time spent by `queries` comes to a
/// day, on the basis their share is on (Culprit's scans where they measured them, the workload's window otherwise),
/// graded as `severity_for_time` grades an amount. For a check that grades the share its own way.
pub fn amount_severity_of(snapshot: &Snapshot, queries: &[&QueryStat], share: f64) -> Severity {
    match scans_time_per_day(snapshot, queries) {
        Some(a_day) => severity_for_amount(a_day * share),
        None => {
            let total = snapshot.workload.as_ref().map_or(0.0, |w| w.total_time_ms);
            severity_for_time_of(snapshot, queries, share * total, 1.0)
        }
    }
}

/// What a figure kept only in a statement's totals is over, where it sits beside the statement's figures over Culprit's
/// scans (its share, runs and average, [`culprit_core::Workload::scans_of`]): "the 1277 days since statistics were
/// reset". A part of its time (its slow runs' excess, a spill, its waits), the rows or temp files a run handles: the
/// source keeps them only in its totals, so each figure's basis is named and the two aren't read as one period. `None`
/// where the statement's figures are the totals' (no scans measured it), or for a recent entry, whose totals are mostly
/// those scans' runs.
pub fn totals_basis(snapshot: &Snapshot, q: &QueryStat) -> Option<String> {
    let w = snapshot.workload.as_ref()?;
    w.scans_of(q).filter(|s| !s.is_recent(q))?;
    Some(w.window_phrase())
}

/// The hours [`severity_for_recent_time`] rates statement `q`'s recent time over: `hours`, or for runs that came in
/// bursts at least a day, so their total is never extrapolated to a day it didn't run.
pub fn burst_hours(snapshot: &Snapshot, q: &QueryStat, hours: f64) -> f64 {
    if ran_in_bursts(snapshot, q) { hours.max(24.0) } else { hours }
}

/// [`severity_for_time`] for time spent over the last `hours` of the window (since a plan took over, since a change):
/// the amount a day over those hours, or under an hour their own total, so a regression that began this morning isn't
/// diluted by the week before it.
pub fn severity_for_recent_time(hours: f64, time_ms: f64, share: f64) -> Severity {
    let a_day = if hours >= culprit_core::RATE_MIN_HOURS { time_ms * 24.0 / hours } else { time_ms };
    severity_for_share(share).min(severity_for_amount(a_day))
}

fn severity_for_amount(a_day_ms: f64) -> Severity {
    if a_day_ms >= HIGH_MS_A_DAY {
        Severity::High
    } else if a_day_ms >= MEDIUM_MS_A_DAY {
        Severity::Medium
    } else {
        Severity::Low
    }
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
    // Each statement's share as every surface states it (`Workload::statement_share`): over Culprit's scans where they
    // measured it, otherwise weighed as the source allows (a plan cached for a month against one compiled an hour ago).
    let share: f64 = queries.iter().map(|q| workload.statement_share(q)).sum();
    let per_day_ms = if queries.iter().any(|q| workload.scans_of(q).is_some()) {
        // What the scans measured a day, beside the others' a day over the window.
        let days: Vec<f64> = queries.iter().filter_map(|q| workload.statement_time_per_day(q)).collect();
        (!days.is_empty()).then(|| days.iter().sum())
    } else {
        // A day's worth only of runs that came that way: a burst's or a single run's time spread over the window is a
        // rate it never ran at.
        workload.per_day_measured(total_ms).filter(|_| queries.iter().any(|q| workload.runs_per_day_of(q).is_some()))
    };
    Some(QueryImpact { share: share.min(1.0), per_day_ms })
}

/// The database's time a day over Culprit's scans, where they measured one of `queries` ([`culprit_core::Workload::scans_of`]): what
/// a share of database time over them is a share of. `None` where none of them is read by the scans.
pub fn scans_time_per_day(snapshot: &Snapshot, queries: &[&QueryStat]) -> Option<f64> {
    let w = snapshot.workload.as_ref()?;
    queries.iter().find_map(|q| w.scans_of(q)).map(|s| s.per_day(s.workload_time_ms))
}

/// The grade of a share of database time over Culprit's scans ([`scans_time_per_day`]): the share, and the amount a
/// day it is of what the database ran in them. `None` where none of `queries` is read by the scans.
pub fn severity_over_scans(snapshot: &Snapshot, queries: &[&QueryStat], share: f64) -> Option<Severity> {
    let a_day = scans_time_per_day(snapshot, queries)? * share;
    Some(severity_for_share(share).min(severity_for_amount(a_day)))
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

/// Whether an object name as an engine writes it (`[app].[orders]`, `"public"."orders"`, `db.dbo.orders`, `orders`)
/// names `schema.table`.
pub fn names_table(object: &str, schema: &str, table: &str) -> bool {
    let o = object.to_ascii_lowercase().replace(['[', ']', '"', '`'], "");
    let qualified = format!("{schema}.{table}").to_ascii_lowercase();
    o == qualified || o.ends_with(&format!(".{qualified}")) || o == table.to_ascii_lowercase()
}

/// Where a statement's first SELECT, INSERT, UPDATE, DELETE or MERGE is, as a whole word, and which it is: past any
/// parameter declarations and comments the engine kept with the text (Query Store's `(@p0 int)UPDATE …`).
pub fn first_verb(text: &str) -> Option<(usize, &'static str)> {
    let lower = text.to_ascii_lowercase();
    ["select", "insert", "update", "delete", "merge"]
        .into_iter()
        .filter_map(|k| {
            lower
                .match_indices(k)
                .find(|(at, _)| {
                    let before = lower[..*at].chars().next_back();
                    let after = lower[at + k.len()..].chars().next();
                    !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                        && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .map(|(at, _)| (at, k))
        })
        .min_by_key(|(at, _)| *at)
}

/// The table an INSERT, UPDATE, DELETE or MERGE names as the one it changes, as written (`dbo.orders`), unquoted and
/// lower-cased: past a `WITH` list, `TOP (n)`, INTO, FROM and ONLY, and an alias (T-SQL's `UPDATE o … FROM orders o`)
/// resolved to its table. `None` for a read, a table variable, or anything else.
pub fn write_target(text: &str, engine: Engine) -> Option<String> {
    // MySQL's digest text spaces the dot of a qualified name: `shop` . `orders`.
    let text = text.replace("` . `", "`.`");
    let main = main_statement(&text);
    let (at, verb) = first_verb(main)?;
    if verb == "select" {
        return None;
    }
    let mut rest = main[at + verb.len()..].trim_start();
    loop {
        let word = rest.chars().take_while(|c| c.is_ascii_alphabetic() || *c == '_').collect::<String>();
        match word.to_ascii_lowercase().as_str() {
            "into" | "from" | "only" | "low_priority" | "quick" | "ignore" | "delayed" | "high_priority" => {
                rest = rest[word.len()..].trim_start();
            }
            // TOP (1000), TOP 50, TOP (@n) PERCENT
            "top" => {
                rest = rest[word.len()..].trim_start();
                let end = if rest.starts_with('(') {
                    rest.find(')').map_or(rest.len(), |i| i + 1)
                } else {
                    rest.find(char::is_whitespace).unwrap_or(rest.len())
                };
                rest = rest[end..].trim_start();
                if rest.get(..7).is_some_and(|w| w.eq_ignore_ascii_case("percent")) {
                    rest = rest[7..].trim_start();
                }
            }
            _ => break,
        }
    }
    let token: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | '"' | '`' | '#' | '$' | '@'))
        .collect();
    let name = token
        .split('.')
        .map(|part| part.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`')))
        .collect::<Vec<_>>()
        .join(".")
        .to_ascii_lowercase();
    if name.is_empty() || name.starts_with('@') {
        return None;
    }
    if !name.contains('.')
        && let Some((_, table)) = culprit_core::sql::table_aliases(main, engine).into_iter().find(|(a, _)| *a == name)
    {
        return Some(table);
    }
    Some(name)
}

/// The tables `q` changes rows of, as named: those its plan's modifying steps write, or, with no such step in a
/// captured plan, the target its INSERT, UPDATE, DELETE or MERGE names ([`write_target`]). A table it only reads (in a
/// join, a subquery, an EXISTS) isn't one.
pub fn write_targets(q: &QueryStat, engine: Engine) -> Vec<String> {
    let modified: Vec<String> = q
        .plan
        .iter()
        .flat_map(|p| p.nodes())
        .filter(|n| n.kind == culprit_core::NodeKind::Modify)
        .filter_map(|n| n.object.clone())
        .collect();
    if !modified.is_empty() {
        return modified;
    }
    write_target(&q.text, engine).into_iter().collect()
}

/// Whether `q` changes rows of `schema.table` ([`write_targets`]), rather than only reading it.
pub fn writes_table(q: &QueryStat, engine: Engine, schema: &str, table: &str) -> bool {
    write_targets(q, engine).iter().any(|o| names_table(o, schema, table))
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
        // SQLite's catalog, and the bookkeeping tables frameworks read on every boot or migration.
        "sqlite_master",
        "sqlite_schema",
        "sqlite_temp_master",
        "pragma_",
        "sqlite_version(",
        "__efmigrationshistory",
        "schema_migrations",
        "ar_internal_metadata",
        "django_migrations",
    ]
    .iter()
    .any(|marker| t.contains(marker))
        || t.trim_start().starts_with("pragma ")
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

/// Text for a `--` comment line of fix SQL. A database's names can hold line breaks (a quoted identifier may), and one
/// would end the comment and turn the rest of the name into SQL that whoever runs the fix executes: every control
/// character and line or paragraph separator becomes a space. Every name or value a comment line carries goes through it.
pub fn comment_text(text: &str) -> String {
    text.chars().map(|c| if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') { ' ' } else { c }).collect()
}

/// The engine's quoting (`Engine::dialect`).
pub fn quote(engine: Engine, ident: &str) -> String {
    engine.dialect().quote(ident)
}

pub fn qualified(engine: Engine, schema: &str, name: &str) -> String {
    engine.dialect().qualified(&[schema, name])
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
    // Azure SQL runs every edition's features; RDS runs the edition it was bought with, read below.
    if snapshot.server.platform().is_some_and(|p| p.is_azure()) {
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
    index_sql(snapshot, schema, table, keys, include, Some(filter))
}

/// A new index on `schema.table` in the engine's dialect, online where the server's edition can build it so.
pub fn create_index_sql(snapshot: &Snapshot, schema: &str, table: &str, keys: &[String], include: &[String]) -> String {
    index_sql(snapshot, schema, table, keys, include, None)
}

fn index_sql(
    snapshot: &Snapshot,
    schema: &str,
    table: &str,
    keys: &[String],
    include: &[String],
    filter: Option<&str>,
) -> String {
    let engine = snapshot.server.engine;
    let dialect = engine.dialect();
    let name = index_name(table, keys);
    let name = if engine == Engine::Mssql { name } else { name.to_ascii_lowercase() };
    let online = if supports_online_index(snapshot) { culprit_core::Online::Yes } else { culprit_core::Online::No };
    dialect.create_index(&culprit_core::NewIndex {
        target: &dialect.qualified(&[schema, table]),
        unique: false,
        name: &name,
        keys,
        include,
        filter,
        online,
    })
}

pub fn drop_index_sql(engine: Engine, schema: &str, table: &str, index: &str) -> String {
    engine.dialect().drop_index(schema, table, index)
}

/// Days the cumulative counters (index usage, table stats) have been accumulating. One source for every window that
/// says so: the counters' own age as the scan measured it, else the server's start time where counters start with the
/// server. Not on PostgreSQL: its counters outlive a clean restart and are discarded by a crash recovery that keeps the
/// start time, so only the age its scan measured (from the last reset or discard) says how long they have counted.
pub fn counter_days(snapshot: &Snapshot) -> Option<f64> {
    if let Some(hours) = snapshot.pressure.as_ref().and_then(|p| p.uptime_hours) {
        return Some((hours / 24.0).max(0.0));
    }
    if snapshot.server.engine == Engine::Postgres {
        return None;
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
    // Counters that may have started long after the server did have no length to divide by: said as counted.
    if per_day_is_measured(days) && !counters_may_be_reset(snapshot) {
        let per_day = total / days;
        format!("about {} {} a day", format::count(per_day), if per_day.round() == 1.0 { one } else { many })
    } else {
        format!("{} {} {}", format::count(total), if total == 1.0 { one } else { many }, since_phrase(snapshot))
    }
}

/// [`per_day_is_measured`] for counters of this snapshot: also never on counters that may have been reset since the
/// server started ([`counters_may_be_reset`]), whose length is unknown.
pub fn counters_measure_a_day(snapshot: &Snapshot, days: f64) -> bool {
    per_day_is_measured(days) && !counters_may_be_reset(snapshot)
}

/// Minutes performance_schema's statement digests may begin after the server started before its counters count as
/// possibly reset: the application's first statements come within moments of a start.
const PS_RESET_MINUTES: f64 = 10.0;

/// Whether MySQL's or MariaDB's performance_schema counters (table and index I/O, as well as statements) may have
/// started after the server did: its statement digests begin well after the start. They were truncated then (setup
/// scripts and `sys.ps_truncate_all_tables()` truncate the table and index counters with them), or no statement ran
/// before; either way the counters' length is unknown, so they are named "since the server started or its
/// performance_schema counters were last reset", with no span and no rate a day.
pub fn counters_may_be_reset(snapshot: &Snapshot) -> bool {
    let Some(w) =
        snapshot.workload.as_ref().filter(|w| w.kind() == Some(culprit_core::WorkloadKind::PerformanceSchema))
    else {
        return false;
    };
    let started = snapshot.server.started_at.as_deref().and_then(parse_epoch_seconds);
    let digests = w.window_start.as_deref().and_then(parse_epoch_seconds);
    snapshot.server.engine == Engine::Mysql
        && started.zip(digests).is_some_and(|(started, digests)| digests - started > PS_RESET_MINUTES * 60.0)
}

/// Days the usage counters have surely counted, for a claim of absence (an index never read, a table never deleted
/// from): their age ([`counter_days`]), or, where performance_schema's may have been reset since the server started
/// ([`counters_may_be_reset`]), the days its statement digests have counted: they start over together, so the server's
/// uptime, which a reset leaves as it was, says nothing about how long the counters have watched.
pub fn watched_days(snapshot: &Snapshot) -> Option<f64> {
    if counters_may_be_reset(snapshot) {
        return snapshot.workload.as_ref().map(|w| (w.window_hours / 24.0).max(0.0));
    }
    counter_days(snapshot)
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
    // Truncated with the statements (`sys.ps_truncate_all_tables`), the table and index counters' start is unknown:
    // named for what it may be, with no span.
    if counters_may_be_reset(snapshot) {
        return "since the server started or its performance_schema counters were last reset".to_string();
    }
    let since = if counters_since_reset(snapshot) {
        "since its statistics were last reset"
    } else {
        "since the server last restarted"
    };
    match counter_days(snapshot) {
        Some(days) if days >= 2.0 => format!("in the {} days {since}", days_counted(days)),
        Some(days) if days * 24.0 < 1.0 => {
            let minutes = (days * 24.0 * 60.0).round().max(1.0);
            if minutes == 1.0 { format!("in the minute {since}") } else { format!("in the {minutes} minutes {since}") }
        }
        Some(days) if days * 24.0 < 1.5 => format!("in the hour {since}"),
        Some(days) => format!("in the {} hours {since}", (days * 24.0).round()),
        None => since.to_string(),
    }
}

/// Days counted, never more than were: a tenth under 10 (9.51 days are "9.5", not "10"), whole days from 10 on. One
/// implementation for prose, metrics (`format::days`) and the app's `format.ts`.
pub use culprit_core::format::days_counted;

/// "a read from Orders", "an UPDATE of Users" — a short handle for a query in titles.
pub fn query_handle(q: &QueryStat) -> String {
    if let Some(object) = &q.object_name {
        return format!("a statement in {object}");
    }
    statement_handle(&q.text)
}

/// The statement a `WITH` list leads to: `WITH b AS (SELECT …) DELETE FROM jobs …` is a delete, not a read. The text
/// from the first verb outside the common table expressions' parentheses and strings; any other text as it is.
pub fn main_statement(text: &str) -> &str {
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    if !text.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("with")) {
        return text;
    }
    let bytes = text.as_bytes();
    let (mut depth, mut quote, mut i) = (0i32, None::<u8>, 4);
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0
                    && c.is_ascii_alphabetic()
                    && !bytes[i - 1].is_ascii_alphanumeric()
                    && bytes[i - 1] != b'_' =>
                {
                    let word: String =
                        text[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if matches!(
                        word.to_ascii_lowercase().as_str(),
                        "select" | "insert" | "update" | "delete" | "merge" | "values"
                    ) {
                        return &text[i..];
                    }
                    i += word.len();
                    continue;
                }
                _ => {}
            },
        }
        i += 1;
    }
    text
}

/// `query_handle` for bare statement text.
pub fn statement_handle(text: &str) -> String {
    if let Some(table) = statistics_maintenance_table(text) {
        return format!("a statistics refresh on {table}");
    }
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    // A driver's or a pool's statement (BEGIN, COMMIT, DISCARD ALL, SELECT 1) is named as it is: it is short, and
    // "a query" would hide what sends it.
    if culprit_core::sql::is_session_statement(text) && text.len() <= 40 {
        return text.trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ");
    }
    // A write behind a WITH list is named as the write.
    let main = main_statement(text);
    if main.len() != text.len() && !main.get(..6).is_some_and(|w| w.eq_ignore_ascii_case("select")) {
        return statement_handle(main);
    }
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
    let mut rest = text[at..].trim_start();
    if verb == "update"
        && rest.get(..3).is_some_and(|s| s.eq_ignore_ascii_case("top"))
        && rest[3..].starts_with(|c: char| c.is_whitespace() || c == '(')
    {
        let after = rest[3..].trim_start();
        let tail = if after.starts_with('(') {
            after.find(')').map(|end| &after[end + 1..])
        } else {
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0 && after[digits..].starts_with(char::is_whitespace)).then(|| &after[digits..])
        };
        if let Some(tail) = tail {
            rest = tail.trim_start();
            if rest.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("percent"))
                && rest[7..].starts_with(char::is_whitespace)
            {
                rest = rest[7..].trim_start();
            }
        }
    }
    let token: String =
        rest.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | '"' | '`')).collect();
    let name = token.rsplit('.').next()?.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`')).to_string();
    (!name.is_empty() && name != "(").then_some(name)
}

/// UPDATE STATISTICS is optimizer maintenance, not an UPDATE of application rows.
pub(crate) fn statistics_maintenance_table(text: &str) -> Option<String> {
    culprit_core::sql::mssql_statistics_target(text)
}

/// "once", never "1 times": a count of runs in a sentence.
fn times(n: f64) -> String {
    match format::count(n.round()).as_str() {
        "1" => "once".to_string(),
        count => format!("{count} times"),
    }
}

/// "about 48K times a day" over a day or more of history, otherwise the real count: extrapolating 35 minutes to a day
/// misleads, and so does "0.2 times a day" for one run in six days.
pub fn runs_phrase(snapshot: &Snapshot, executions: f64) -> String {
    match &snapshot.workload {
        Some(w) if w.window_hours >= 20.0 && w.per_day(executions) < 1.0 => {
            format!("{} in {}", times(executions), w.window_phrase())
        }
        Some(w) if w.window_hours >= 20.0 => format!("about {} a day", times(w.per_day(executions))),
        // A window nobody can date (a log without timestamps, counters with no reset time) isn't "1 minute".
        Some(w) if w.window_unknown() => format!("{} over {}", times(executions), w.window_phrase()),
        Some(w) => format!("{} in {}", times(executions), culprit_core::span_words(w.window_hours)),
        None => times(executions),
    }
}

/// How often statement `q` ran, for a sentence that names its window right after it ("It ran {} at 2 ms on average:
/// 25% of all database time in the last 6 days"): the count and, where it's part of the answer, when
/// ([`culprit_core::Workload::cadence`]). A rate a day only for runs that came that way: "750 times" and ", on Oct 11,
/// 15:51–15:55 UTC" for a burst, the count alone for fewer runs than the window has days.
pub fn statement_runs(snapshot: &Snapshot, q: &QueryStat) -> (String, String) {
    let Some(w) = snapshot.workload.as_ref() else { return (times(q.executions), String::new()) };
    let cadence = w.cadence(q);
    let when = match (cadence.when(), &cadence) {
        (Some(when), culprit_core::Cadence::Burst { .. }) => format!(", {when}"),
        (Some(when), _) => format!(" {when}"),
        (None, _) => String::new(),
    };
    let count = match cadence {
        culprit_core::Cadence::Daily(per_day) | culprit_core::Cadence::DailyWithin { per_day, .. } => {
            format!("about {} a day", times(per_day))
        }
        culprit_core::Cadence::Burst { runs, .. } => times(runs),
        // The window the sentence names holds them.
        culprit_core::Cadence::Counted(runs) if w.window_hours >= culprit_core::PER_DAY_MIN_HOURS => times(runs),
        culprit_core::Cadence::Counted(runs) => runs_phrase(snapshot, runs),
    };
    (count, when)
}

/// How often statement `q` ran, said whole for a title or a sentence that names no window after it: "30K times on Oct
/// 17 at 10:17 UTC" for a burst, "120K times on Oct 11 and Oct 17" for runs on a few days, "about 4.2K times a day"
/// where its runs came that way, otherwise [`runs_phrase`]'s count and window ("4.0K times in 2 hours").
pub fn runs_of(snapshot: &Snapshot, q: &QueryStat) -> String {
    let Some(w) = snapshot.workload.as_ref() else { return times(q.executions) };
    let cadence = w.cadence(q);
    // On a source of running totals, what Culprit's scans measured where they did: "about 31K times a day over 89
    // hours of Culprit's scans".
    if let Some(s) = w.scans_of(q).filter(|_| !matches!(cadence, culprit_core::Cadence::Burst { .. })) {
        let per_day = s.per_day(s.runs);
        let runs = if per_day >= 1.0 { format!("about {} a day", times(per_day)) } else { times(s.runs) };
        return format!("{runs} over {}", s.phrase());
    }
    match (&cadence, cadence.when()) {
        (culprit_core::Cadence::Burst { runs, .. }, Some(when)) => format!("{} {when}", times(*runs)),
        (culprit_core::Cadence::Daily(per_day) | culprit_core::Cadence::DailyWithin { per_day, .. }, when) => {
            format!("about {} a day{}", times(*per_day), when.map(|w| format!(" {w}")).unwrap_or_default())
        }
        _ => runs_phrase(snapshot, q.executions),
    }
}

/// How often statements ran together, said whole: [`runs_of`] for one; for several a rate a day only where every one
/// of them ran that way ([`culprit_core::Workload::runs_per_day_of`]), otherwise what was counted and over how long.
pub fn runs_of_all(snapshot: &Snapshot, queries: &[&QueryStat]) -> String {
    let runs: f64 = queries.iter().map(|q| q.executions).sum();
    match (queries, snapshot.workload.as_ref()) {
        ([q], _) => runs_of(snapshot, q),
        (_, Some(w))
            if w.window_hours >= culprit_core::PER_DAY_MIN_HOURS
                && queries.iter().any(|q| w.runs_per_day_of(q).is_none()) =>
        {
            format!("{} in {}", times(runs), w.window_phrase())
        }
        _ => runs_phrase(snapshot, runs),
    }
}

/// Whether statement `q`'s runs came in bursts ([`culprit_core::Cadence::Burst`]): said by when they came, graded on
/// their total.
pub fn ran_in_bursts(snapshot: &Snapshot, q: &QueryStat) -> bool {
    snapshot.workload.as_ref().is_some_and(|w| w.cadence(q).is_burst())
}

/// What Culprit's own scans measured of `q` on a source of running totals, as the sentence that says what it costs,
/// where they measured it ([`culprit_core::Workload::scans_of`]): what runs now, with the totals since the source began
/// counting beside it where those differ twofold ([`culprit_core::Workload::totals_beside`]). `of` names what the share
/// is of ("of all database time"). "It ran about 1.7K times a day at 4.0 ms on average over 74 hours of Culprit's own
/// scans: 43% of all database time in them (about 388 runs a day and 2.6 ms on average over the 1277 days since
/// statistics were reset)."
pub fn scans_sentence(snapshot: &Snapshot, q: &QueryStat, of: &str) -> Option<String> {
    let w = snapshot.workload.as_ref()?;
    let s = w.scans_of(q)?;
    // Runs that came in bursts within the scans are said by when they came, never a rate a day: "It ran 750 times at
    // 5.4 ms on average, on Oct 11, 15:51–15:55 UTC: 3% of all database time in 37 hours of Culprit's own scans".
    if let Some(when) = Some(w.cadence(q)).filter(|c| c.is_burst()).and_then(|c| c.when()) {
        return Some(format!(
            "It ran {} at {}{}, {when}: {} {of} in {} of Culprit's own scans{}.",
            times(q.executions),
            format::duration(s.mean_ms().unwrap_or(q.mean_time_ms)),
            if q.executions > 1.0 { " on average" } else { "" },
            format::percent(s.share()),
            s.length(),
            w.totals_beside(q, &s).map(|t| format!(" ({t})")).unwrap_or_default()
        ));
    }
    let per_day = s.per_day(s.runs);
    let runs = if per_day >= 1.0 { format!("about {} a day", times(per_day)) } else { times(s.runs) };
    Some(format!(
        "It ran {runs} at {}{} over {} of Culprit's own scans: {} {of} in them{}.",
        format::duration(s.mean_ms().unwrap_or(q.mean_time_ms)),
        if s.runs > 1.0 { " on average" } else { "" },
        s.length(),
        format::percent(s.share()),
        w.totals_beside(q, &s).map(|t| format!(" ({t})")).unwrap_or_default()
    ))
}

/// The distinct statements behind the top three active findings.
fn top_queries<'a>(
    snapshot: &'a Snapshot,
    findings: impl IntoIterator<Item = &'a culprit_core::Finding>,
) -> Vec<&'a QueryStat> {
    let Some(w) = snapshot.workload.as_ref() else { return Vec::new() };
    let ids: std::collections::BTreeSet<&str> = findings
        .into_iter()
        .filter(|f| !f.inactive)
        .take(3)
        .flat_map(|f| f.queries.iter().map(|q| q.id.as_str()))
        .collect();
    w.queries.iter().filter(|q| ids.contains(q.id.as_str())).collect()
}

/// Share of database time behind the top three active findings: their distinct queries, each counted once, so
/// two findings about the same query don't double it. The one number both the summary and the dashboard state. A
/// statement Culprit's scans measured counts at its share over them ([`culprit_core::Workload::statement_share`]).
pub fn top_findings_share<'a>(
    snapshot: &'a Snapshot,
    findings: impl IntoIterator<Item = &'a culprit_core::Finding>,
) -> Option<f64> {
    let w = snapshot.workload.as_ref()?;
    let queries = top_queries(snapshot, findings);
    if queries.is_empty() {
        return None;
    }
    let share: f64 = queries
        .iter()
        .map(|q| if w.scans_of(q).is_some() { w.statement_share(q) } else { w.share(q.total_time_ms) })
        .sum();
    Some(share.min(1.0))
}

/// What the share behind the top three active findings ([`top_findings_share`]) is over, when Culprit's scans measured
/// every statement behind them: "89 hours of Culprit's scans". `None` otherwise: it is over the workload's window.
pub fn top_findings_scans<'a>(
    snapshot: &'a Snapshot,
    findings: impl IntoIterator<Item = &'a culprit_core::Finding>,
) -> Option<String> {
    let w = snapshot.workload.as_ref()?;
    let spans: Option<Vec<culprit_core::ScanSpan>> =
        top_queries(snapshot, findings).iter().map(|q| w.scans_of(q)).collect();
    spans?.into_iter().max_by(|a, b| a.hours.total_cmp(&b.hours)).map(|s| s.phrase())
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

/// [`rate_per_day`] for a part of statements' counts (`value` of `queries`' runs, time, rows or log), at the pace their
/// runs came ([`culprit_core::Workload::rate_per_day_of`]): a burst's total, never extrapolated to a day; runs on a few
/// days over those days. With several statements, each one's part in proportion to its time (its runs where none has
/// any). Every statement-level threshold a day (a "busy" bar, an amount behind a severity) is judged on it.
pub fn rate_per_day_of(snapshot: &Snapshot, queries: &[&QueryStat], value: f64) -> Option<f64> {
    let w = snapshot.workload.as_ref()?;
    let (time, runs): (f64, f64) = queries.iter().fold((0.0, 0.0), |(t, r), q| (t + q.total_time_ms, r + q.executions));
    if queries.is_empty() {
        return w.rate_per_day(value);
    }
    queries
        .iter()
        .map(|q| {
            let part = match (time > 0.0, runs > 0.0) {
                (true, _) => q.total_time_ms / time,
                (false, true) => q.executions / runs,
                _ => 1.0 / queries.len() as f64,
            };
            w.rate_per_day_of(q, value * part)
        })
        .sum()
}

/// Whether statement `q`'s `total` (its runs, rows, log…), counted over the workload window, comes to at least `per_day`
/// a day, judged at the rate it ran while it ran ([`rate_per_day_of`], over at least an hour of data), or, in a window
/// shorter than a day, a total that is already that many (the day that window sits in held at least as many). Nothing
/// is extrapolated: a burst passes a "busy" bar only with as many in its own day, and runs on a few days with as many a
/// day of them. 120K runs in four 2-hour stretches on two days are busy; 4,000 in six seconds aren't "57.8K a day", and
/// 14 runs in a minute aren't "20K a day". False when the window is unknown.
pub fn at_least_per_day_of(snapshot: &Snapshot, q: &QueryStat, total: f64, per_day: f64) -> bool {
    let Some(w) = snapshot.workload.as_ref() else { return false };
    if w.window_unknown() {
        return false;
    }
    rate_per_day_of(snapshot, &[q], total).is_some_and(|r| r >= per_day)
        || (w.window_hours > 0.0 && w.window_hours <= 24.0 && total >= per_day)
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

    /// A name a database allows (a quoted identifier with a line break in it) can't end a comment of fix SQL and run the
    /// rest of itself as a statement.
    #[test]
    fn a_name_in_a_comment_stays_in_the_comment() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
            let hostile = "orders\nDROP TABLE users;\r\n--";
            let line = format!("-- read {}", comment_text(&qualified(engine, "app", hostile)));
            assert_eq!(line.lines().count(), 1, "{engine:?}: {line:?}");
            assert!(line.starts_with("-- read "), "{line}");
        }
        assert_eq!(comment_text("a\u{2028}b\u{85}c\u{2029}d\te"), "a b c d e");
        assert_eq!(comment_text("plain [app].[orders]"), "plain [app].[orders]");
    }

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

    // The aptabase readings: random_page_cost can be set for one database, which RDS allows at once and without a
    // custom parameter group; the parameter group re-plans every database on the instance. Settings only a superuser
    // sets per database (track_io_timing), and those that are the server's alone (pg_stat_statements.max), stay the
    // platform's.
    #[test]
    fn a_per_database_setting_is_set_for_this_database_first_on_a_managed_server() {
        let mut server = crate::waits::snapshot_with(Engine::Postgres).server;
        server.cloud = Some("Amazon RDS".into());
        let change = setting_change(&server, "random_page_cost", "1.1");
        assert_eq!(change.sql.as_deref(), Some("ALTER DATABASE demo SET random_page_cost = 1.1;"));
        assert!(change.how.starts_with("Set random_page_cost to 1.1 for this database only"), "{}", change.how);
        assert!(change.how.contains("sessions that connect after the change use it"), "{}", change.how);
        assert!(change.how.ends_with("set it in the instance's DB parameter group."), "{}", change.how);
        for name in ["track_io_timing", "pg_stat_statements.max", "autovacuum"] {
            let change = setting_change(&server, name, "on");
            assert!(change.sql.is_none() && change.how.contains("DB parameter group"), "{name}: {}", change.how);
        }
        // A name that needs quoting is quoted.
        server.database = "Shop-EU".into();
        let change = setting_change(&server, "work_mem", "64MB");
        assert_eq!(change.sql.as_deref(), Some("ALTER DATABASE \"Shop-EU\" SET work_mem = '64MB';"));
        // A server you run takes ALTER SYSTEM, as before; MySQL keeps no settings per database.
        server.cloud = None;
        let change = setting_change(&server, "random_page_cost", "1.1");
        assert!(change.sql.unwrap().starts_with("ALTER SYSTEM SET random_page_cost = 1.1;"));
        let mut mysql = crate::waits::snapshot_with(Engine::Mysql).server;
        mysql.cloud = Some("Amazon RDS".into());
        let change = setting_change(&mysql, "sort_buffer_size", "4194304");
        assert!(change.sql.is_none() && change.how.contains("DB parameter group"), "{}", change.how);
    }

    // Theme A's note on missing-index: a request's grade took the statements' share over Culprit's scans with the time
    // their totals hold. A share is graded on the basis it is on: over the scans, the amount a day it is of what the
    // database ran in them.
    #[test]
    fn a_share_over_the_scans_is_graded_on_their_amount_not_the_totals() {
        let path = format!("{}/../../fixtures/postgres-demo.json", env!("CARGO_MANIFEST_DIR"));
        let mut s: Snapshot = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("snapshot");
        let w = s.workload.as_mut().expect("workload");
        // 40 days of totals; 36 hours of scans, in which the database ran 10 s a 12-hour step and the statement 3 s.
        (w.window_start, w.window_hours) = (Some("2026-09-01T11:54:21Z".into()), 960.0);
        w.timeline = Some(
            serde_json::from_value(serde_json::json!({
                "source": "scans",
                "starts": ["2026-10-10T00:00:00Z", "2026-10-10T12:00:00Z", "2026-10-11T00:00:00Z"],
                "end": "2026-10-11T12:00:00Z",
                "time_ms": [10000.0, 10000.0, 10000.0],
                "executions": [5000.0, 5000.0, 5000.0]
            }))
            .expect("timeline"),
        );
        let q = &mut w.queries[0];
        q.trend = Some(culprit_core::QueryTrend { time_ms: vec![Some(3_000.0); 3], executions: vec![Some(10.0); 3] });
        // Its totals: 1,000 runs of a minute since the reset, 25 minutes a day and 30% of the database's.
        (q.executions, q.mean_time_ms, q.total_time_ms) = (1_000.0, 60_000.0, 60_000_000.0);
        w.total_time_ms = 200_000_000.0;
        let q = s.workload.as_ref().unwrap().queries[0].clone();
        assert!(s.workload.as_ref().unwrap().scans_of(&q).is_some_and(|s| !s.is_recent(&q)));
        // 30% of the scans' time is 6 s a day of a database that ran 20 s a day in them: Low, however much time the
        // totals hold; the totals' time beside the scans' share made it High.
        assert_eq!(severity_for_statement_share(&s, &[&q], 0.3), Severity::Low);
        assert_eq!(amount_severity_of(&s, &[&q], 0.3), Severity::Low);
        assert_eq!(severity_for_time_of(&s, &[&q], q.total_time_ms, 0.3), Severity::High, "the old mix");
        // Without the scans, the totals are the basis of both.
        let mut totals = s.clone();
        totals.workload.as_mut().unwrap().timeline = None;
        assert_eq!(severity_for_statement_share(&totals, &[&q], 0.3), Severity::High);
    }

    // D's open gap (MY-5): performance_schema's table counters start over with its statements, so "no writes" waits a
    // week of the digests' own counting, however long the server has been up; then it is said as counted.
    #[test]
    fn no_writes_on_counters_that_may_be_reset_waits_a_week_of_their_own() {
        let path = format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR"));
        let mut s: Snapshot = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("snapshot");
        let taken = parse_epoch_seconds(&s.taken_at).expect("taken_at");
        s.server.started_at = Some(culprit_core::format::iso_from_epoch(taken - 10.0 * 86_400.0));
        s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(240.0);
        let digests = |s: &mut Snapshot, days: f64| {
            let w = s.workload.as_mut().expect("workload");
            w.window_start = Some(culprit_core::format::iso_from_epoch(taken - days * 86_400.0));
            w.window_hours = days * 24.0;
        };
        digests(&mut s, 2.0);
        assert!(counters_may_be_reset(&s) && watched_days(&s) == Some(2.0));
        let cost = crate::indexes::write_cost(&s, "shop", "orders").expect("a cost");
        assert!(cost.contains("in what its counters cover, which may be short"), "{cost}");
        digests(&mut s, 8.0);
        let cost = crate::indexes::write_cost(&s, "shop", "orders").expect("a cost");
        assert_eq!(
            cost,
            "shop.orders received no writes since the server started or its performance_schema counters were last \
             reset, so this index would cost next to nothing to maintain."
        );
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
        // A staging SQL Server reading: 9.51 days of counters were "the 10 days", more than they counted.
        assert_eq!(at("2026-10-02T04:50:36Z"), "in the 9.5 days since the server last restarted");
        assert_eq!(at("2026-10-02T05:05:00Z"), "in the 9.5 days since the server last restarted");
        assert_eq!(at("2026-10-09T17:05:00Z"), "in the 2 days since the server last restarted");
        assert_eq!(at("2026-09-28T23:53:00Z"), "in the 12 days since the server last restarted");
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
        q.text = "UPDATE STATISTICS dbo.[Orders] WITH SAMPLE 100 PERCENT".into();
        assert_eq!(query_handle(&q), "a statistics refresh on Orders");
        q.text = "UPDATE\nSTATISTICS dbo.[Orders]".into();
        assert_eq!(query_handle(&q), "a statistics refresh on Orders");
        q.text = "(@P1 int) UPDATE STATISTICS dbo.[Orders]".into();
        assert_eq!(query_handle(&q), "a statistics refresh on Orders");
        assert!(statistics_maintenance_table("UPDATE dbo.Statistics SET value = 1").is_none());
        assert!(statistics_maintenance_table("UPDATE statistics_log SET value = 1").is_none());
        assert!(statistics_maintenance_table("UPDATE statistics SET value = 1").is_none());
        assert!(statistics_maintenance_table("UPDATE statistics s SET value = 1").is_none());
        assert!(statistics_maintenance_table("UPDATE statistics AS s SET value = 1").is_none());
        q.text = "UPDATE TOP (1000) dbo.[Orders] SET state = 1".into();
        assert_eq!(query_handle(&q), "an update of Orders");
        q.text = "UPDATE TOP (@limit) dbo.[Orders] SET state = 1".into();
        assert_eq!(query_handle(&q), "an update of Orders");
        q.text = "UPDATE TOP 10 PERCENT dbo.[Orders] SET state = 1".into();
        assert_eq!(query_handle(&q), "an update of Orders");
        q.text = "UPDATE top100 SET state = 1".into();
        assert_eq!(query_handle(&q), "an update of top100");
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
            rows_affected: None,
            plan_id: None,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct PredicateColumns {
    pub equality: Vec<String>,
    pub range: Vec<String>,
}

/// Comparisons a default b-tree index can seek, as (operator, equality). Not-equal (`<>`, `!=`) and PostgreSQL's
/// LIKE (`~~`, `~~*`) are left out on purpose: see [`predicate_columns`].
const OPERATORS: [(&str, bool); 10] = [
    (" = ANY ", true),
    (" IN ", true),
    (" BETWEEN ", false),
    (" LIKE ", false),
    (">=", false),
    ("<=", false),
    ("<=>", true),
    ("=", true),
    ("<", false),
    (">", false),
];

/// Comparisons no default b-tree index serves: a condition that has one names no column to index.
const NOT_SEEKABLE: [&str; 5] = ["<>", "!=", "~~", "!~", " NOT "];

/// Columns compared against a parameter or literal in a plan predicate, e.g. `(customer_id = $1)` or
/// `[db].[dbo].[Orders].[CustomerId] as [m].[CustomerId]=[@P1]`: the columns an index on the filter would seek. Only
/// conditions every row must meet count: a branch of an OR (`status = 'open' OR priority > 3`) doesn't, since an index
/// on one branch's column can't find the other branch's rows. Column-to-column comparisons (joins) are skipped, and so
/// are comparisons a default b-tree index can't seek: not-equal on any engine, NOT anything, a LIKE whose pattern
/// starts with a wildcard, and PostgreSQL's LIKE (`~~`), which needs the C collation or a `text_pattern_ops` index.
pub fn predicate_columns(predicate: &str) -> PredicateColumns {
    let cleaned = strip_casts(&predicate.replace(['[', ']', '"', '`'], ""));
    let mut out = PredicateColumns::default();
    for conjunct in conjuncts(&cleaned) {
        let conjunct = conjunct.replace(['(', ')'], " ");
        let conjunct = conjunct.split_whitespace().collect::<Vec<_>>().join(" ");
        let upper = conjunct.to_ascii_uppercase();
        let unquoted = format!(" {} ", upper.split('\'').step_by(2).collect::<Vec<_>>().join("''"));
        if NOT_SEEKABLE.iter().any(|op| unquoted.contains(op)) {
            continue;
        }
        let Some((at, op, equality)) = OPERATORS
            .iter()
            .filter_map(|(op, eq)| upper.find(op).map(|at| (at, *op, *eq)))
            .min_by_key(|(at, op, _)| (*at, std::cmp::Reverse(op.len())))
        else {
            continue;
        };
        let left = drop_alias(conjunct[..at].trim());
        let right = drop_alias(conjunct[at + op.len()..].trim());
        // `LIKE '%abc'` reads every key: only a pattern with a fixed start can seek.
        if op == " LIKE " && starts_with_wildcard(right) {
            continue;
        }
        let column = match (column_name(left), column_name(right)) {
            (Some(c), None) if is_value(right) => c,
            (None, Some(c)) if is_value(left) && op != " LIKE " && op != " BETWEEN " => c,
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

/// The conditions every row must meet: the parts of a predicate joined by AND at its top level, nested ANDs taken
/// apart, and any part that holds an OR dropped (its rows can come from either branch). `x BETWEEN a AND b` stays one
/// condition. Quoted text is never split.
fn conjuncts(text: &str) -> Vec<String> {
    let text = without_outer_parens(text.trim());
    let mut merged: Vec<String> = Vec::new();
    let mut between = false;
    for part in split_top_level(text, " AND ") {
        // BETWEEN's own AND joins its bounds back.
        if between && let Some(last) = merged.last_mut() {
            last.push_str(" AND ");
            last.push_str(part);
            between = false;
            continue;
        }
        between = split_top_level(part, " BETWEEN ").len() == 2;
        merged.push(part.to_string());
    }
    if merged.len() == 1 {
        let only = &merged[0];
        // An OR anywhere in a condition (`a = 1 OR b = 2`, `NOT (a = 1 OR …)`) leaves no column every row must match.
        return if contains_keyword(only, " OR ") { Vec::new() } else { vec![only.clone()] };
    }
    merged.iter().flat_map(|part| conjuncts(part)).collect()
}

/// `((a = 1))` → `a = 1`: parentheses around the whole text only.
fn without_outer_parens(mut text: &str) -> &str {
    while text.starts_with('(') && text.ends_with(')') && closing_paren(text) == Some(text.len() - 1) {
        text = text[1..text.len() - 1].trim();
    }
    text
}

/// Where the parenthesis opening `text` closes, skipping quoted text.
fn closing_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `text` split at `separator` (an ASCII keyword with its spaces, matched case-insensitively) where it stands outside
/// parentheses and quotes.
fn split_top_level<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let upper = text.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start, mut i) = (0i32, false, 0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            // Bytes, not a string slice: `i` can be inside a multi-byte character, where slicing would panic.
            _ if !quoted && depth == 0 && bytes[i..].starts_with(separator.as_bytes()) => {
                parts.push(&text[start..i]);
                i += separator.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&text[start..]);
    parts
}

/// Whether `keyword` (with its spaces) appears outside quotes, at any depth.
fn contains_keyword(text: &str, keyword: &str) -> bool {
    let unquoted: String = text.split('\'').step_by(2).collect::<Vec<_>>().join("''");
    unquoted.replace(['(', ')'], " ").to_ascii_uppercase().contains(keyword)
}

/// `'%abc'`, `N'_bc'`: a LIKE pattern that doesn't start with fixed text.
fn starts_with_wildcard(pattern: &str) -> bool {
    let p = pattern.trim();
    let p = p.strip_prefix(['N', 'n']).filter(|rest| rest.starts_with('\'')).unwrap_or(p);
    p.strip_prefix('\'').is_some_and(|rest| rest.starts_with(['%', '_']))
}

/// A predicate without PostgreSQL's casts. A cast of a column to another type (`(created_at)::date`) hides it from its index like a function would: it stays
/// as ` ::cast` so the side is no bare column and gives no key. Casts of values, and to text (how PostgreSQL prints a
/// varchar column compared with a string), are harmless and go.
fn strip_casts(text: &str) -> String {
    const MULTIWORD: [&str; 4] = [" with time zone", " without time zone", " varying", " precision"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("::") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let ty: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        rest = &after[ty.len()..];
        while let Some(suffix) = MULTIWORD.iter().find(|m| rest.to_ascii_lowercase().starts_with(*m)) {
            rest = &rest[suffix.len()..];
        }
        if let Some(stripped) = rest.strip_prefix("[]") {
            rest = stripped;
        }
        let harmless = matches!(
            ty.to_ascii_lowercase().as_str(),
            "text" | "varchar" | "character" | "bpchar" | "name" | "unknown"
        );
        if !harmless && cast_operand_is_column(&out) {
            out.push_str(" ::cast");
        }
    }
    out.push_str(rest);
    out
}

/// Whether what `before` ends with, the operand of a cast that follows it, is a column: `(created_at)` or
/// `o.created_at`, not `'2026-01-01'`, `$1`, `now()` or a number.
fn cast_operand_is_column(before: &str) -> bool {
    let trimmed = before.trim_end();
    let operand = if let Some(inner) = trimmed.strip_suffix(')') {
        let mut depth = 1;
        let open = inner.char_indices().rev().find(|(_, c)| {
            match c {
                ')' => depth += 1,
                '(' => depth -= 1,
                _ => {}
            }
            depth == 0
        });
        match open {
            // `now()`: a function call's parentheses, not a parenthesized column.
            Some((at, _)) if inner[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') => return false,
            Some((at, _)) => inner[at + 1..].trim(),
            None => return false,
        }
    } else {
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '$')))
            .map_or(0, |(i, c)| i + c.len_utf8());
        &trimmed[start..]
    };
    !is_value(operand) && column_name(operand).is_some()
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
        // MySQL writes typed literals in plans: TIMESTAMP'2026-09-05 00:00:00'.
        || ["TIMESTAMP'", "DATE'", "TIME'"].iter().any(|p| upper.starts_with(p))
        || upper.starts_with("ARRAY")
        || upper.starts_with("{")
        || upper.starts_with("CONVERT_IMPLICIT") && (upper.contains("@") || upper.contains("$"))
        || upper.starts_with("NOW")
        || upper.starts_with("CURRENT_")
        || upper.starts_with("GETDATE")
        || upper.starts_with("SYSUTCDATETIME")
        || is_value_call(s)
}

/// A function of values alone is a value: `dateadd(day,@P1,'2026-01-01')` (or the same with its parentheses gone, as a
/// condition is read), `coalesce($1, 0)`, `date_sub(?, INTERVAL ? DAY)`. Every argument is a parameter, a literal, a
/// date part (`day`) or such a call, and at least one is a parameter or a literal. A function of a column
/// (`upper(name)`, `dateadd(day, 1, created_at)`) isn't.
fn is_value_call(s: &str) -> bool {
    let s = s.trim();
    let name_len = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')).unwrap_or(s.len());
    if name_len == 0 || name_len == s.len() || !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    // A call's name is followed by its parenthesis (or the space left where it was): a name that goes on with other
    // characters is a name, such as a column qualified by a database called `app-db-01` (not `app` minus a value).
    if !s[name_len..].starts_with(['(', ' ']) {
        return false;
    }
    let rest = s[name_len..].trim();
    let inner = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).unwrap_or(rest);
    let mut any_value = false;
    for arg in top_level_args(inner) {
        let arg = arg.trim();
        let upper = arg.to_ascii_uppercase();
        let interval = upper.strip_prefix("INTERVAL ").and_then(|r| r.split_whitespace().next()).is_some_and(is_value);
        if is_value(arg) || interval {
            any_value = true;
        } else if !DATE_PARTS.contains(&upper.as_str()) {
            return false;
        }
    }
    any_value
}

/// The date parts SQL Server's date functions take as a bare word (`dateadd(day, …)`), and the units of an interval.
const DATE_PARTS: [&str; 38] = [
    "YEAR",
    "YY",
    "YYYY",
    "QUARTER",
    "QQ",
    "Q",
    "MONTH",
    "MM",
    "M",
    "DAYOFYEAR",
    "DY",
    "Y",
    "DAY",
    "DD",
    "D",
    "WEEK",
    "WK",
    "WW",
    "WEEKDAY",
    "DW",
    "W",
    "HOUR",
    "HH",
    "MINUTE",
    "MI",
    "N",
    "SECOND",
    "SS",
    "S",
    "MILLISECOND",
    "MS",
    "MICROSECOND",
    "MCS",
    "NANOSECOND",
    "NS",
    "ISO_WEEK",
    "ISOWK",
    "ISOWW",
];

/// `text` split at its commas outside quotes and parentheses: a call's arguments.
fn top_level_args(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start) = (0i32, false, 0);
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    #[test]
    fn statements_without_a_table_are_named() {
        assert_eq!(statement_handle("DO SLEEP(?)"), "a call to SLEEP");
        assert_eq!(statement_handle("DISCARD ALL"), "DISCARD ALL");
        // A write behind a WITH list is the write, wherever its CTEs read.
        assert_eq!(statement_handle("WITH b AS (SELECT id FROM jobs) DELETE FROM jobs USING b"), "a delete from jobs");
        assert_eq!(statement_handle("WITH b AS (SELECT 'delete' FROM x) UPDATE t SET a = 1"), "an update of t");
        assert_eq!(statement_handle("SELECT sleep(?)"), "a call to sleep");
        assert_eq!(statement_handle("SELECT pg_sleep($1)"), "a call to pg_sleep");
        assert_eq!(statement_handle("DO `SLEEP` (?)"), "a call to SLEEP");
        assert_eq!(statement_handle("CALL `refresh_totals` (?)"), "a call to refresh_totals");
        assert_eq!(statement_handle("EXEC [dbo].[usp_orders] @id = 1"), "a call to dbo.usp_orders");
        assert_eq!(statement_handle("CALL shop.refresh(?)"), "a call to shop.refresh");
        assert_eq!(statement_handle("CALL refresh_totals(?)"), "a call to refresh_totals");
        assert_eq!(statement_handle("SELECT id FROM orders WHERE id = ?"), "a read from orders");
        // A pool's ping is named as it is.
        assert_eq!(statement_handle("SELECT 1"), "SELECT 1");
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

    // A function of parameters and constants is a value (the planted shipped_at range on SQL Server), and a function of
    // a column isn't.
    #[test]
    fn a_function_of_values_is_a_value() {
        let p = predicate_columns(
            "[culprit_demo].[shop].[orders].[shipped_at]>=dateadd(day,[@P1],'2026-01-01 00:00:00.000') AND \
             [culprit_demo].[shop].[orders].[shipped_at]<dateadd(day,[@P2],'2026-01-01 00:00:00.000')",
        );
        assert_eq!(p.range, vec!["shipped_at"]);
        assert_eq!(predicate_columns("(total > COALESCE($1, 0))").range, vec!["total"]);
        assert_eq!(predicate_columns("(`o`.`created_at` > DATE_SUB(?, INTERVAL ? DAY))").range, vec!["created_at"]);
        assert_eq!(predicate_columns("([o].[due]>dateadd(day,(1),[o].[created_at]))"), PredicateColumns::default());
        assert_eq!(predicate_columns("(name = upper(other_name))"), PredicateColumns::default());
    }

    // An index on one branch of an OR can't find the other branch's rows, and a default b-tree can't seek not-equal,
    // NOT, a pattern starting with a wildcard, or PostgreSQL's LIKE.
    #[test]
    fn only_conditions_an_index_can_seek_name_columns() {
        let p = predicate_columns("((status)::text = 'open'::text) OR (priority > 3)");
        assert_eq!(p, PredicateColumns::default());
        let p = predicate_columns("[db].[dbo].[t].[status]='open' OR [db].[dbo].[t].[priority]>(3)");
        assert_eq!(p, PredicateColumns::default());
        // A condition every row meets still counts next to an OR.
        let p = predicate_columns("((customer_id = $1) AND ((status = 'open') OR (priority > 3)))");
        assert_eq!(p.equality, vec!["customer_id"]);
        assert!(p.range.is_empty());
        // Parentheses decide: `a OR (b AND c)` requires nothing of every row.
        let p = predicate_columns("((a = 1) OR ((b = 2) AND (c = 3)))");
        assert_eq!(p, PredicateColumns::default());
        // The false flag: a cast of the column to date hides it from an index on it, like date(created_at) would.
        let p = predicate_columns("((created_at)::date = $1)");
        assert!(p.equality.is_empty() && p.range.is_empty(), "{p:?}");
        let p =
            predicate_columns("((amount)::integer > 10) AND (created_at > '2026-01-01'::date) AND (id = $1::bigint)");
        assert_eq!((p.equality, p.range), (vec!["id".to_string()], vec!["created_at".to_string()]));
        let p = predicate_columns("((status)::text <> 'closed'::text)");
        assert_eq!(p, PredicateColumns::default());
        let p = predicate_columns("((email)::text ~~ 'abc%'::text)");
        assert_eq!(p, PredicateColumns::default());
        let p = predicate_columns("(NOT (customer_id = $1))");
        assert_eq!(p, PredicateColumns::default());
        let p = predicate_columns("(shop.orders.status not in ('a','b'))");
        assert_eq!(p, PredicateColumns::default());
        // SQL Server and MySQL seek a LIKE with a fixed start, never one that starts with a wildcard.
        let p = predicate_columns("[db].[dbo].[t].[email] like N'abc%'");
        assert_eq!(p.range, vec!["email"]);
        let p = predicate_columns("[db].[dbo].[t].[email] like N'%abc'");
        assert_eq!(p, PredicateColumns::default());
        // MySQL: typed literals, BETWEEN and the null-safe equal.
        let p = predicate_columns(
            "((shop.orders.status = 'shipped') and (shop.orders.created_at >= TIMESTAMP'2026-09-05 00:00:00'))",
        );
        assert_eq!((p.equality, p.range), (vec!["status".to_string()], vec!["created_at".to_string()]));
        let p = predicate_columns("(shop.o.total between 10 and 20) and (shop.o.customer_id <=> 11272)");
        assert_eq!((p.equality, p.range), (vec!["customer_id".to_string()], vec!["total".to_string()]));
        // Quoted text is never taken apart.
        let p = predicate_columns("(note = 'a OR b') AND (kind = 'x <> y')");
        assert_eq!(p.equality, vec!["note", "kind"]);
    }

    // A staging SQL Server reading: a Key Lookup's residual on a column qualified by a database whose name has hyphens
    // read as a call of values (`app` minus …), so the lookups that found nothing lost their reading and their fix.
    #[test]
    fn a_column_qualified_by_a_hyphenated_database_is_a_column() {
        let p = predicate_columns(
            "[app-db-01].[app].[call_sessions].[conversation_id]=CONVERT_IMPLICIT(uniqueidentifier,[@par2],0)",
        );
        assert_eq!(p.equality, vec!["conversation_id"]);
        // A call of values stays a value, with or without its parentheses.
        assert!(is_value("dateadd(day,@P1,'2026-01-01')") && is_value("dateadd day,@P1,'2026-01-01'"));
    }
}

pub use culprit_core::SettingChange;

/// How to change a server setting where this server runs. On a server you manage it is SQL, as the engine spells it
/// (`EngineSpec::change_setting`: `ALTER SYSTEM` and a reload on PostgreSQL, `SET PERSIST`/`SET GLOBAL` on MySQL and
/// MariaDB…); a managed service refuses those, so there it is the platform's own control (a DB parameter group, server
/// parameters, database flags) and no SQL. A setting the engine keeps per database, which the database's owner may set
/// (`EngineSpec::database_setting`: PostgreSQL's `random_page_cost`, `work_mem`), is set for this database first there:
/// it changes nothing else on the instance and needs no parameter group or reboot; the platform's control is the
/// route for every database. Every check that suggests a setting words it with this, so advice is always something the
/// reader can run.
pub fn setting_change(server: &culprit_core::ServerInfo, name: &str, value: &str) -> SettingChange {
    // A managed service changes server settings in its own control, not with SQL (`Platform::settings_control`).
    if let Some(control) = server.platform().and_then(|p| p.settings_control()) {
        if let Some(mut database) = server.engine.spec().database_setting(server, name, value) {
            database.how.push_str(&format!(" To change it for every database on the server, set it in {control}."));
            return database;
        }
        return SettingChange { how: format!("Set {name} to {value} in {control}."), sql: None };
    }
    server.engine.spec().change_setting(server, name, value)
}

/// Live shows who holds a lock only while someone waits for it: waits that come less than once an hour over the data's
/// window (`waits` of them, at least) are too few to count on catching one. A window that can't be dated says nothing
/// of how often, and isn't too rare.
pub fn too_rare_to_catch(snapshot: &Snapshot, waits: f64) -> bool {
    snapshot
        .workload
        .as_ref()
        .filter(|w| !w.window_unknown() && w.window_hours > 0.0)
        .is_some_and(|w| waits.ceil() / w.window_hours < 1.0)
}

/// What to find a lock wait's holder with ([`find_lock_holder`]), and the SQL that sets it up where that is SQL.
pub struct LockHolder {
    pub lead: String,
    pub sql: Option<String>,
}

/// How to see the next lock waits too rare to catch live ([`too_rare_to_catch`]), and, with `slower_than` (ms), the
/// next runs slower than that, whatever they waited for: the engine's own log, set the way settings are set on this
/// server, or what the engine already keeps. "Have the next ones logged instead. …", and its SQL where it has one.
pub fn log_next_waits(snapshot: &Snapshot, slower_than: Option<f64>) -> LockHolder {
    let server = &snapshot.server;
    let on = |name: &str| snapshot.setting(name).is_some_and(|s| crate::config::is_on(&s.value));
    match server.engine {
        Engine::Postgres => {
            let deadlock = match snapshot.setting("deadlock_timeout").map(|s| s.value.trim().to_string()) {
                Some(v) if v.parse::<f64>().is_ok() => format!("{v} ms"),
                Some(v) => v,
                None => "1 s by default".into(),
            };
            let threshold = slower_than.map(|ms| if ms >= 1_000.0 { format!("{}s", ms / 1_000.0) } else { format!("{ms}ms") });
            let mut changes: Vec<(&str, &str)> = Vec::new();
            if !on("log_lock_waits") {
                changes.push(("log_lock_waits", "on"));
            }
            if let Some(t) = threshold.as_deref() {
                changes.push(("log_min_duration_statement", t));
            }
            let locks = format!(
                "log_lock_waits {} each lock wait longer than deadlock_timeout ({deadlock}) with the process holding the lock",
                if on("log_lock_waits") { "is on already, and logs" } else { "then logs" }
            );
            let slow = threshold
                .as_deref()
                .map(|t| format!(", and log_min_duration_statement each statement slower than {t} (any statement, not only this one) with its duration"))
                .unwrap_or_default();
            if changes.is_empty() {
                return LockHolder {
                    lead: format!("{}: the server log has the next ones, with the process holding the lock.", capitalize_first(&locks)),
                    sql: None,
                };
            }
            let change = crate::config::settings_change(server, &changes);
            LockHolder { lead: format!("Have the next ones logged instead. {} {locks}{slow}.", change.how), sql: change.sql }
        }
        Engine::Mysql => {
            // A run worth logging: the bar asked for, else the second the slow-log finding suggests; never a raise.
            let seconds = slower_than.map_or(1.0, |ms| ms / 1_000.0);
            let current = snapshot.setting("long_query_time").and_then(|s| s.value.trim().parse::<f64>().ok());
            let logging = on("slow_query_log") || on("log_slow_query");
            let mut changes: Vec<(&str, String)> = Vec::new();
            if !logging {
                changes.push(("slow_query_log", "ON".into()));
            }
            if current.is_none_or(|c| c > seconds) {
                changes.push(("long_query_time", format!("{seconds}")));
            }
            let bar = format!("{}", current.filter(|c| *c <= seconds).unwrap_or(seconds));
            let mariadb = if server.is_mariadb() { format!(" {}", crate::config::MARIADB_TABLE_LOCK_TIME) } else { String::new() };
            let keeps = |verb: &str| {
                format!(
                    "The slow query log {verb} each statement slower than {bar} s with its lock time and when it ran; {} keeps no record of who held the lock, so match those times with what the application ran then.{mariadb}",
                    server.name()
                )
            };
            if changes.is_empty() {
                return LockHolder {
                    lead: format!("Look for the next ones in the slow query log. {}", keeps("already keeps")),
                    sql: None,
                };
            }
            let pairs: Vec<(&str, &str)> = changes.iter().map(|(n, v)| (*n, v.as_str())).collect();
            let change = crate::config::settings_change(server, &pairs);
            LockHolder {
                lead: format!("Have the next ones logged instead. {} {}", change.how, keeps("then keeps")),
                sql: change.sql,
            }
        }
        // Query Store keeps when a query waited, interval by interval, not who held the lock. On a server whose options
        // can be set, the blocked process report names the holder.
        Engine::Mssql if server.platform().is_some_and(|p| p.is_azure_sql_database()) => LockHolder {
            lead: "Query Store keeps when it waited, interval by interval (sys.query_store_wait_stats), not who held the lock: match the intervals of its waits with what the application ran then.".into(),
            sql: None,
        },
        Engine::Mssql => LockHolder {
            lead: "Have the next ones reported instead: SQL Server's blocked process report names the session holding the lock, with its statement, for each wait longer than the server's blocked process threshold (s) (0, off, by default; sp_configure sets it). Capture sqlserver.blocked_process_report in an event session to keep them.".into(),
            sql: None,
        },
        Engine::Sqlite => LockHolder {
            lead: "SQLite keeps no record of what held the file's write lock: the application's own logs of its writes at those times say which one it was.".into(),
            sql: None,
        },
    }
}

/// How to find what holds the locks statement `q` waits for, in the words every check that names a lock wait uses: Live
/// while it waits, where its waits come at least once an hour (`waits`, at least; every run without it); otherwise the
/// next ones logged ([`log_next_waits`]); and for a statement that no longer runs, nothing to catch until it comes back.
pub fn find_lock_holder(snapshot: &Snapshot, q: &QueryStat, waits: Option<f64>) -> LockHolder {
    let live = "Find what holds the locks while it waits (Live shows the blocking chain).";
    let rare = too_rare_to_catch(snapshot, waits.unwrap_or(q.executions));
    // What logs the next ones, by name: no settings are given for a statement that ran once or no longer runs.
    let log = match snapshot.server.engine {
        Engine::Postgres => "log_lock_waits",
        Engine::Mysql => "the slow query log, with its lock time",
        Engine::Mssql if snapshot.server.platform().is_some_and(|p| p.is_azure_sql_database()) => {
            "Query Store's waits for each interval date them"
        }
        Engine::Mssql => "SQL Server's blocked process report names the holder",
        Engine::Sqlite => "the application's own log of its writes",
    };
    // A statement that ran once is history (`ran_once`): nothing is set up for it unless it runs again.
    if crate::ran_once(q) {
        let window = snapshot.workload.as_ref().map(|w| w.window_phrase()).unwrap_or_else(|| "the window".into());
        return LockHolder {
            lead: format!(
                "It ran once in {window}, so there is no wait to catch now; if it runs again, have its lock waits logged ({log}) to see what holds them."
            ),
            sql: None,
        };
    }
    if crate::query_stopped(snapshot, q).is_some() {
        return LockHolder {
            lead: if rare {
                format!(
                    "It no longer runs, so there is no wait to catch now; if it comes back, have its lock waits logged ({log}): its runs are too few to catch one while it waits."
                )
            } else {
                "It no longer runs, so there is no wait to catch now; if it comes back, find what holds the locks while it waits (Live shows the blocking chain).".into()
            },
            sql: None,
        };
    }
    if !rare {
        return LockHolder { lead: live.into(), sql: None };
    }
    let logged = log_next_waits(snapshot, None);
    let window = snapshot.workload.as_ref().map(|w| w.window_phrase()).unwrap_or_default();
    let few = match waits {
        Some(n) => format!(
            "Its waits can come from as few as {} runs in {window}, too few to count on catching one while it happens.",
            format::count(n.ceil())
        ),
        None => format!("It ran {}, too seldom to count on catching it while it waits.", runs_of(snapshot, q)),
    };
    LockHolder { lead: format!("{few} {}", logged.lead), sql: logged.sql }
}

/// A statistic a plan was compiled with, as SQL Server's plan XML names it (its `StatisticsInfo`, kept in the plan's raw
/// XML; the parser keeps nothing else of it): unquoted names.
pub struct PlanStatistic {
    pub schema: String,
    pub table: String,
    pub name: String,
}

/// The statistics `plan` was compiled with ([`PlanStatistic`]): none for a plan without raw XML, or another engine's.
pub fn plan_statistics(plan: &culprit_core::Plan) -> Vec<PlanStatistic> {
    let Some(raw) = plan.raw.as_deref() else { return Vec::new() };
    let attr = |tag: &str, key: &str| -> Option<String> {
        let at = tag.find(&format!(" {key}=\""))? + key.len() + 3;
        Some(tag[at..].split('"').next()?.trim_matches(['[', ']']).to_string())
    };
    raw.split("<StatisticsInfo ")
        .skip(1)
        .filter_map(|rest| {
            let tag = format!(" {}", rest.split('>').next()?);
            Some(PlanStatistic {
                schema: attr(&tag, "Schema")?,
                table: attr(&tag, "Table")?,
                name: attr(&tag, "Statistics")?,
            })
        })
        .collect()
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
