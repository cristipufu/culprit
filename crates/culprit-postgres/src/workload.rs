//! Statement counters, families and safe plan capture from pg_stat_statements.
use crate::{
    plan_json,
    probe::{Probe, StatementsAccess},
    rows::{TAG, iso, query, query_one},
};
use culprit_core::{
    Engine, PlanKind, QueryFamily, QueryStat, Result, Setting, SnapshotOptions, Spread, WaitStat, Workload,
};
use tokio_postgres::Client;

pub(crate) struct CollectedWorkload {
    pub(crate) workload: Workload,
    pub(crate) plan_note: Option<String>,
    /// Why no statement was planned, when the login may not plan here (`crate::trust`).
    pub(crate) plans_withheld: Option<culprit_core::Error>,
    pub(crate) families: Vec<QueryFamily>,
    /// pg_stat_statements' own counters (entries evicted since reset), surfaced as status settings.
    pub(crate) status: Vec<Setting>,
    /// Disk read/write time across all statements (zero unless track_io_timing is on).
    pub(crate) io_ms: f64,
}

pub(crate) async fn collect_workload(
    client: &Client,
    probe: &Probe,
    options: &SnapshotOptions,
) -> Result<Option<CollectedWorkload>> {
    let StatementsAccess::Usable { relation, info } = &probe.statements else {
        return Ok(None);
    };
    let base = statements_base(relation, probe.version_num);
    match collect_with_base(client, probe, options, &base, info.as_deref()).await {
        // An extension left on its pre-13 column names after pg_upgrade.
        Err(e) if e.code == culprit_core::ErrorCode::InvalidInput && probe.version_num >= 130_000 => {
            collect_with_base(client, probe, options, &statements_base_legacy(relation), info.as_deref()).await
        }
        other => other,
    }
}

async fn collect_with_base(
    client: &Client,
    probe: &Probe,
    options: &SnapshotOptions,
    base: &str,
    reset: Option<&str>,
) -> Result<Option<CollectedWorkload>> {
    let totals = query_one(
        client,
        &format!("SELECT coalesce(sum(total_ms), 0) AS total_ms, coalesce(sum(calls), 0) AS calls, count(*) AS n, coalesce(sum(io_ms), 0) AS io_ms FROM ({base}) s"),
    )
    .await?;
    let (total_ms, total_calls, count, io_ms) = totals
        .map(|r| {
            (
                r.number("total_ms").unwrap_or(0.0),
                r.number("calls").unwrap_or(0.0),
                r.number("n").unwrap_or(0.0),
                r.number("io_ms").unwrap_or(0.0),
            )
        })
        .unwrap_or_default();

    let (window_start, window_end, window_hours) = window(client, reset).await?;

    let n = options.top_queries.max(1) as f64;
    let by_time = (n * 0.6).ceil() as u32;
    let by_other = (n * 0.2).ceil() as u32;
    let top_sql = format!(
        "WITH s AS ({base}),
         r AS (SELECT s.*,
                 row_number() OVER (ORDER BY total_ms DESC) AS by_time,
                 row_number() OVER (ORDER BY calls DESC) AS by_calls,
                 row_number() OVER (ORDER BY reads DESC) AS by_reads
               FROM s WHERE id IS NOT NULL AND query <> '<insufficient privilege>')
         SELECT * FROM r WHERE by_time <= {by_time} OR by_calls <= {by_other} OR by_reads <= {by_other}
         ORDER BY total_ms DESC LIMIT {limit}",
        limit = options.top_queries.max(1)
    );
    let rows = query(client, &top_sql).await?;
    let mut queries: Vec<QueryStat> = rows
        .iter()
        .map(|r| QueryStat {
            resolved_write_target: None,
            requests: None,
            id: r.string("id"),
            query_hash: None,
            text: r.string("query"),
            parameters: None,
            object_name: None,
            executions: r.number("calls").unwrap_or(0.0),
            total_time_ms: r.number("total_ms").unwrap_or(0.0),
            mean_time_ms: r.number("mean_ms").unwrap_or(0.0),
            max_time_ms: r.number("max_ms"),
            stddev_time_ms: r.number("stddev_ms"),
            cpu_time_ms: None,
            rows: r.number("rows"),
            logical_reads: r.number("reads"),
            physical_reads: r.number("phys"),
            temp_writes: r.number("temp"),
            first_seen: r.text("first_seen"),
            last_seen: None,
            plans: Vec::new(),
            plan: None,
            // Disk time is the only part of a statement's time PostgreSQL measures per statement.
            waits: r
                .number("io_ms")
                .filter(|ms| *ms > 0.0)
                .map(|ms| {
                    vec![WaitStat {
                        category: "IO".into(),
                        wait_type: Some("data file reads and writes".into()),
                        wait_ms: ms,
                        tasks: None,
                    }]
                })
                .unwrap_or_default(),
            trend: None,
            spread: r.number("min_ms").map(|min_ms| Spread { min_ms: Some(min_ms), ..Default::default() }),
            costs: None,
            // PostgreSQL's `rows` counts rows returned and rows changed alike.
            rows_affected: None,
            plan_id: None,
        })
        .collect();
    // The text of each statement as this login ran it, when it did: the only text an elevated login plans, so what is
    // planned is what passed that check, never another role's copy of the same statement.
    let own: std::collections::HashMap<String, String> =
        rows.iter().filter_map(|r| Some((r.string("id"), r.text("own_query")?))).collect();
    let (plan_note, plans_withheld) = if probe.generic_plans() {
        match attach_plans(client, &mut queries, &own, probe, options.plans).await {
            Ok(note) => (note, None),
            Err(withheld) => (None, Some(withheld)),
        }
    } else {
        (None, None)
    };
    // Families and eviction counts are extras: a failure leaves them out, never the workload.
    let families = query_families(client, base).await.unwrap_or_default();
    let mut status = Vec::new();
    if reset.is_some()
        && let Some(info) = base_relation(base).map(|r| format!("{r}_info"))
        && let Ok(Some(row)) = query_one(client, &format!("SELECT dealloc FROM {info}")).await
        && let Some(dealloc) = row.number("dealloc")
    {
        status.push(Setting {
            name: "pg_stat_statements.dealloc".into(),
            value: format!("{dealloc:.0}"),
            default_value: None,
            scope: "status".into(),
        });
    }

    Ok(Some(CollectedWorkload {
        workload: Workload {
            history_inputs: Vec::new(),
            measurement: None,
            source: "pg_stat_statements".into(),
            kind: Some(culprit_core::WorkloadKind::StatementStats),
            windowed: false,
            window_start,
            window_end,
            window_hours,
            total_time_ms: total_ms,
            total_cpu_ms: None,
            total_executions: total_calls,
            query_count: count,
            queries,
            timeline: None,
            untracked_time_ms: None,
            hourly_time_ms: None,
            min_logged_ms: None,
            log: None,
        },
        plan_note,
        plans_withheld,
        families,
        status,
        io_ms,
    }))
}

/// When pg_stat_statements started counting. 14+ says (`pg_stat_statements_info.stats_reset`). Before that only a
/// server that discards its statistics at shutdown (`pg_stat_statements.save = off`) bounds it: they started at the
/// last start or later. With `save` on (the default) they outlive restarts, so the start time says nothing: the window
/// is unknown (`window_start` empty, 0 hours) and Culprit's earlier scans of the database narrow it down.
async fn window(client: &Client, reset: Option<&str>) -> Result<(Option<String>, Option<String>, f64)> {
    let start = match reset {
        Some(reset) => format!("'{reset}'::timestamptz"),
        None => {
            "CASE WHEN current_setting('pg_stat_statements.save', true) = 'off' THEN pg_postmaster_start_time() END"
                .to_string()
        }
    };
    let window_sql = format!(
        "SELECT {start_text} AS start, {now} AS end_at, extract(epoch FROM now() - ({start})) / 3600 AS hours",
        start_text = iso(&start),
        now = iso("now()")
    );
    let window = query_one(client, &window_sql).await?;
    Ok(window
        .map(|r| {
            let start = r.text("start");
            let hours = if start.is_some() { r.number("hours").unwrap_or(0.0) } else { 0.0 };
            (start, r.text("end_at"), hours)
        })
        .unwrap_or((None, None, 0.0)))
}

/// The pg_stat_statements relation a base query reads (`"public".pg_stat_statements`).
fn base_relation(base: &str) -> Option<&str> {
    let from = base.find(" FROM ")? + " FROM ".len();
    base[from..].split_whitespace().next()
}

/// Statements pg_stat_statements keeps apart only because a value list has a different length (`IN ($1, $2)` vs
/// `IN ($1, $2, $3)`) or a VALUES clause a different number of rows: each is planned and tracked on its own. A family
/// counts the texts that differ before their lists are collapsed: the same text under several statement ids (one per
/// schema a tenant's search_path resolves to) is one variant, not a value-list split.
async fn query_families(client: &Client, base: &str) -> Result<Vec<QueryFamily>> {
    Ok(query(client, &families_sql(base)).await?.iter().map(family_of).collect())
}

fn families_sql(base: &str) -> String {
    format!(
        r#"WITH s AS ({base}),
         t AS (
           SELECT id, calls, total_ms, query, regexp_replace(regexp_replace(query, '\$\d+', '$N', 'g'), '\s+', ' ', 'g') AS shape
           FROM s WHERE id IS NOT NULL AND query <> '<insufficient privilege>'),
         n AS (
           SELECT id, calls, total_ms, query, shape,
             regexp_replace(regexp_replace(shape, '\$N(\s*,\s*\$N)+', '$N', 'g'), '(\(\$N\)\s*,\s*)+\(\$N\)', '($N), …', 'g') AS family
           FROM t)
         SELECT md5(family) AS key, count(DISTINCT shape) AS variants, sum(calls) AS executions, sum(total_ms) AS total_ms,
                (array_agg(query ORDER BY total_ms DESC))[1] AS sample,
                array_to_string((array_agg(id::text ORDER BY total_ms DESC))[1:5], ',') AS ids
         FROM n GROUP BY family HAVING count(DISTINCT shape) >= 5
         ORDER BY count(DISTINCT shape) DESC LIMIT 15"#
    )
}

fn family_of(r: &crate::rows::Row) -> QueryFamily {
    QueryFamily {
        key: r.string("key"),
        sample: r.string("sample"),
        variants: r.number("variants").unwrap_or(0.0),
        executions: r.number("executions").unwrap_or(0.0),
        total_time_ms: r.number("total_ms").unwrap_or(0.0),
        cached_bytes: None,
        query_ids: r.string("ids").split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
    }
}

/// What one pg_stat_statements version calls each column, per entry.
struct EntryColumns {
    total: &'static str,
    mean: &'static str,
    max: &'static str,
    stddev: &'static str,
    min: &'static str,
    io: &'static str,
    /// `stats_since` (17+): when the entry started counting.
    since: Option<&'static str>,
    toplevel: bool,
}

/// One row per statement: pg_stat_statements keeps an entry per (role, database, queryid), so a statement several roles
/// run (an app's pool and its migrations, one login per tenant) is several entries splitting its time. Their counts and
/// times add up; the slowest and fastest run are the extremes over the entries; the spread is pooled from each entry's
/// mean and standard deviation (`Σn(σ²+μ²)/N − μ²`). The text and owner are the busiest entry's.
fn aggregated_base(relation: &str, c: &EntryColumns) -> String {
    let toplevel = if c.toplevel { " AND toplevel" } else { "" };
    let first_seen = c.since.map(|since| iso(&format!("min({since})"))).unwrap_or_else(|| "NULL::text".to_string());
    format!(
        "SELECT queryid::text AS id, (array_agg(query ORDER BY {total} DESC))[1] AS query, sum(calls) AS calls,
                sum({total}) AS total_ms, sum({total}) / nullif(sum(calls), 0)::float8 AS mean_ms,
                max({max}) AS max_ms, min({min}) AS min_ms,
                sqrt(greatest(sum(calls * ({stddev} ^ 2 + {mean} ^ 2)) / nullif(sum(calls), 0)::float8
                              - (sum({total}) / nullif(sum(calls), 0)::float8) ^ 2, 0)) AS stddev_ms,
                sum(rows) AS rows, sum(shared_blks_hit + shared_blks_read) AS reads, sum(shared_blks_read) AS phys,
                sum(temp_blks_written) AS temp, sum({io}) AS io_ms, {first_seen} AS first_seen,
                count(*) AS entries,
                (array_agg(query ORDER BY {total} DESC)
                   FILTER (WHERE pg_get_userbyid(userid) = session_user AND current_user = session_user))[1] AS own_query
         FROM {relation}
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()){toplevel}
           AND query NOT LIKE '%{TAG}'
         GROUP BY queryid",
        total = c.total,
        mean = c.mean,
        max = c.max,
        stddev = c.stddev,
        min = c.min,
        io = c.io,
    )
}

fn statements_base(relation: &str, version_num: u32) -> String {
    let modern = version_num >= 130_000;
    aggregated_base(
        relation,
        &EntryColumns {
            total: if modern { "total_exec_time" } else { "total_time" },
            mean: if modern { "mean_exec_time" } else { "mean_time" },
            max: if modern { "max_exec_time" } else { "max_time" },
            stddev: if modern { "stddev_exec_time" } else { "stddev_time" },
            min: if modern { "min_exec_time" } else { "min_time" },
            io: crate::pressure::statements_io_column(version_num),
            since: (version_num >= 170_000).then_some("stats_since"),
            toplevel: version_num >= 140_000,
        },
    )
}

fn statements_base_legacy(relation: &str) -> String {
    aggregated_base(
        relation,
        &EntryColumns {
            total: "total_time",
            mean: "mean_time",
            max: "max_time",
            stddev: "stddev_time",
            min: "min_time",
            io: "(blk_read_time + blk_write_time)",
            since: None,
            toplevel: false,
        },
    )
}

/// Attach generic plans to the heaviest plannable statements. Returns a note when some could not be planned, or why
/// this login plans none here.
async fn attach_plans(
    client: &Client,
    queries: &mut [QueryStat],
    own: &std::collections::HashMap<String, String>,
    probe: &Probe,
    budget: u32,
) -> Result<Option<String>> {
    // Planning runs the code a statement reaches with this login's privileges (`crate::trust`). A login that can do
    // more than read statistics plans only statements it ran itself, and none while another role's code is in reach;
    // each plan checks again, in its own read-only transaction. Statements are planned under Culprit's own
    // search_path, never the role's that ran them: a statement whose names don't resolve here stays unplanned.
    if probe.elevated {
        let owners = crate::trust::foreign_code(client).await?;
        if !owners.is_empty() {
            return Err(crate::trust::withheld(&owners));
        }
        // Before 14 pg_stat_statements can't tell a statement the login ran from one run inside a function on its
        // behalf (no toplevel): with track = all, another role's trigger code is recorded as this login's own.
        if probe.version_num < 140_000 {
            let track =
                query_one(client, "SELECT pg_catalog.current_setting('pg_stat_statements.track', true) AS track")
                    .await?
                    .and_then(|r| r.text("track"))
                    .unwrap_or_default();
            if track.eq_ignore_ascii_case("all") {
                return Err(culprit_core::Error::new(
                    culprit_core::ErrorCode::PermissionDenied,
                    "Not planned: before PostgreSQL 14, with pg_stat_statements.track = all, statements other roles' functions run on this login's behalf are recorded as its own, and this login can do more than read statistics.",
                )
                .with_hint("Set pg_stat_statements.track = top, or connect with a login that has only pg_monitor, and SELECT on the tables."));
            }
        }
    }
    let mut order: Vec<usize> = (0..queries.len()).collect();
    order.sort_by(|a, b| queries[*b].total_time_ms.total_cmp(&queries[*a].total_time_ms));
    let (mut attempted, mut planned, mut others) = (0u32, 0u32, 0u32);
    for index in order {
        if attempted >= budget {
            break;
        }
        let text = if probe.elevated {
            match own.get(&queries[index].id) {
                Some(text) => text.as_str(),
                None => {
                    others += plannable(&queries[index].text).is_some() as u32;
                    continue;
                }
            }
        } else {
            queries[index].text.as_str()
        };
        let Some(statement) = plannable(text) else { continue };
        attempted += 1;
        let Some(json) = generic_plan_json(client, statement, probe).await else { continue };
        if let Ok(mut plan) = plan_json::parse(&json, PlanKind::Generic, Some(statement)) {
            crate::tables::fill(client, &mut plan).await;
            plan_json::mark_generic(&mut plan);
            plan_json::mark_planned_text(&mut plan, &queries[index].id);
            queries[index].plan = Some(plan);
            planned += 1;
        }
    }
    let mut notes = Vec::new();
    if planned < attempted {
        notes.push(format!(
            "Planned {planned} of {attempted} heavy statements. The rest could not be planned: their recorded text is truncated, is not valid SQL once constants are replaced by placeholders, or uses objects this login cannot see."
        ));
    }
    if others > 0 {
        notes.push(format!(
            "{others} heavy statements other roles ran were left unplanned: this login can do more than read statistics, and planning a statement runs the functions it reaches with this login's privileges. A login with only pg_monitor, and SELECT on the tables, plans them."
        ));
    }
    Ok((!notes.is_empty()).then(|| notes.join(" ")))
}

async fn generic_plan_json(client: &Client, statement: &str, probe: &Probe) -> Option<String> {
    // Statement text was written by whoever ran it; it must lex as exactly one statement before it reaches EXPLAIN.
    let statement = culprit_core::sql::single_statement(statement, Engine::Postgres).ok()?;
    let (version_num, elevated) = (probe.version_num, probe.elevated);
    match crate::generic::plan_json(client, statement, version_num, elevated).await {
        Ok(json) => Some(json),
        Err(_) => {
            let rewritten = cast_typed_placeholders(statement)?;
            let rewritten = culprit_core::sql::single_statement(&rewritten, Engine::Postgres).ok()?;
            crate::generic::plan_json(client, rewritten, version_num, elevated).await.ok()
        }
    }
}

const TYPED_LITERAL_TYPES: &[&str] =
    &["interval", "timestamptz", "timestamp", "date", "time", "uuid", "jsonb", "json", "inet", "cidr", "numeric"];

/// pg_stat_statements turns `interval '7 days'` into `interval $1`, which is not valid SQL. Rewrite those as `$1::interval`.
pub fn cast_typed_placeholders(statement: &str) -> Option<String> {
    let lower = statement.to_ascii_lowercase();
    let bytes = statement.as_bytes();
    let mut out = String::with_capacity(statement.len() + 16);
    let mut i = 0;
    let mut changed = false;
    'scan: while i < bytes.len() {
        let at_word_start = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if at_word_start {
            for kind in TYPED_LITERAL_TYPES {
                if !lower[i..].starts_with(kind) {
                    continue;
                }
                let after = &statement[i + kind.len()..];
                let trimmed = after.trim_start();
                let gap = after.len() - trimmed.len();
                let digits: String = trimmed
                    .strip_prefix('$')
                    .map(|r| r.chars().take_while(char::is_ascii_digit).collect())
                    .unwrap_or_default();
                if gap == 0 || digits.is_empty() {
                    continue;
                }
                out.push_str(&format!("${digits}::{kind}"));
                i += kind.len() + gap + 1 + digits.len();
                changed = true;
                continue 'scan;
            }
        }
        let ch = statement[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    changed.then_some(out)
}

/// The statement text if it is a single SELECT / INSERT / UPDATE / DELETE / MERGE / WITH that EXPLAIN can plan.
pub fn plannable(text: &str) -> Option<&str> {
    let statement = text.trim().trim_end_matches(';').trim_end();
    if statement.contains(';') {
        return None;
    }
    let keyword = leading_keyword(statement)?;
    matches!(keyword.as_str(), "select" | "insert" | "update" | "delete" | "with" | "merge" | "values" | "table")
        .then_some(statement)
}

pub fn leading_keyword(text: &str) -> Option<String> {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix('(') {
            rest = after.trim_start();
        } else {
            break;
        }
    }
    let word: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plannable_statements() {
        assert_eq!(plannable("SELECT * FROM t WHERE id = $1;"), Some("SELECT * FROM t WHERE id = $1"));
        assert_eq!(plannable("/* app */ update t set a = $1"), Some("/* app */ update t set a = $1"));
        assert_eq!(
            plannable("-- hi\nWITH x AS (SELECT 1) SELECT * FROM x"),
            Some("-- hi\nWITH x AS (SELECT 1) SELECT * FROM x")
        );
        assert_eq!(plannable("BEGIN"), None);
        assert_eq!(plannable("SET work_mem = $1"), None);
        assert_eq!(plannable("SELECT 1; DROP TABLE t"), None);
        assert_eq!(plannable("(SELECT 1) UNION (SELECT 2)"), Some("(SELECT 1) UNION (SELECT 2)"));
    }

    #[test]
    fn typed_placeholders_become_casts() {
        assert_eq!(
            cast_typed_placeholders("SELECT 1 FROM t WHERE c > now() - interval $1 AND d = DATE $2").as_deref(),
            Some("SELECT 1 FROM t WHERE c > now() - $1::interval AND d = $2::date")
        );
        assert_eq!(cast_typed_placeholders("SELECT update_date FROM t WHERE id = $1"), None);
    }

    #[test]
    fn one_row_per_statement_whichever_roles_ran_it() {
        let base = statements_base("public.pg_stat_statements", 160_000);
        assert!(base.contains("GROUP BY queryid"), "{base}");
        assert!(base.contains("sum(calls) AS calls") && base.contains("sum(total_exec_time) AS total_ms"));
        assert!(base.contains("max(max_exec_time) AS max_ms") && base.contains("min(min_exec_time) AS min_ms"));
        // Pooled spread: each entry's own and how far its mean sits from the statement's.
        assert!(base.contains("sum(calls * (stddev_exec_time ^ 2 + mean_exec_time ^ 2))"));
        assert!(base.contains(" AND toplevel") && base.contains(&format!("NOT LIKE '%{TAG}'")));
        assert_eq!(base_relation(&base), Some("public.pg_stat_statements"));
        // The login's own text of it, compared by role name (never a role name parsed as an identifier, which an Entra
        // login like someone@example.com isn't), and without a subquery whose FROM would hide the relation.
        assert!(
            base.contains(
                "FILTER (WHERE pg_get_userbyid(userid) = session_user AND current_user = session_user))[1] AS own_query"
            ),
            "{base}"
        );
        assert!(!base.contains("regrole"), "{base}");
        let old = statements_base("public.pg_stat_statements", 120_000);
        assert!(
            old.contains("sum(total_time)") && !old.contains("toplevel") && old.contains("NULL::text AS first_seen")
        );
        assert!(statements_base("s.pg_stat_statements", 170_000).contains("min(stats_since)"));
        assert!(statements_base_legacy("s.pg_stat_statements").contains("GROUP BY queryid"));
    }

    #[test]
    fn a_family_counts_texts_not_statement_ids() {
        // The same text under one id per tenant schema is one variant; a different IN-list length is another.
        let sql = families_sql("SELECT 1");
        assert!(sql.contains("count(DISTINCT shape) AS variants"), "{sql}");
        assert!(sql.contains("HAVING count(DISTINCT shape) >= 5"));
    }
}
