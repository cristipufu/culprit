use culprit_core::{
    Category, Confidence, Engine, Finding, Impact, QueryRef, QueryStat, Severity, Snapshot, Subject, SubjectKind,
    format,
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
    pub per_day_ms: f64,
}

pub fn query_impact(snapshot: &Snapshot, queries: &[&QueryStat]) -> Option<QueryImpact> {
    let workload = snapshot.workload.as_ref()?;
    if queries.is_empty() {
        return None;
    }
    let total_ms: f64 = queries.iter().map(|q| q.total_time_ms).sum();
    Some(QueryImpact { share: workload.share(total_ms), per_day_ms: workload.per_day(total_ms) })
}

pub fn apply_query_impact(f: &mut Finding, impact: &QueryImpact) {
    f.impact.db_time_share = Some(impact.share);
    f.impact.time_ms_per_day = Some(impact.per_day_ms);
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
    let t = q.text.to_ascii_lowercase();
    [
        "sys.dm_",
        "sys.query_store",
        "query_store_",
        "sys.objects",
        "sys.columns",
        "information_schema.",
        "pg_catalog.",
        "pg_stat_",
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
                "CREATE INDEX {}\n    ON {target} ({cols})\n    ALGORITHM=INPLACE, LOCK=NONE;",
                quote(engine, &name.to_ascii_lowercase())
            )
        }
    }
}

pub fn drop_index_sql(engine: Engine, schema: &str, table: &str, index: &str) -> String {
    match engine {
        Engine::Mssql => format!("DROP INDEX {} ON {};", quote(engine, index), qualified(engine, schema, table)),
        Engine::Postgres => format!("DROP INDEX CONCURRENTLY {};", qualified(engine, schema, index)),
        Engine::Mysql => format!(
            "DROP INDEX {} ON {} ALGORITHM=INPLACE, LOCK=NONE;",
            quote(engine, index),
            qualified(engine, schema, table)
        ),
    }
}

/// Days the cumulative counters (index usage, table stats) have been accumulating.
pub fn counter_days(snapshot: &Snapshot) -> Option<f64> {
    let started = snapshot.server.started_at.as_deref()?;
    let days = days_between(started, &snapshot.taken_at)?;
    Some(days.max(0.0))
}

pub fn days_between(from: &str, to: &str) -> Option<f64> {
    let a = parse_epoch_seconds(from)?;
    let b = parse_epoch_seconds(to)?;
    Some((b - a) / 86_400.0)
}

/// Minimal RFC 3339 / ISO 8601 parser (UTC offsets honoured) so checks stay dependency-free.
pub fn parse_epoch_seconds(text: &str) -> Option<f64> {
    let t = text.trim();
    let (date, rest) = t.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let (time, offset_seconds) = split_offset(rest);
    let mut tp = time.split(':');
    let hour: f64 = tp.next()?.parse().ok()?;
    let minute: f64 = tp.next().unwrap_or("0").parse().ok()?;
    let second: f64 = tp.next().unwrap_or("0").parse().ok()?;
    let days = days_from_civil(year, month, day);
    Some(days as f64 * 86_400.0 + hour * 3600.0 + minute * 60.0 + second - offset_seconds)
}

fn split_offset(time: &str) -> (&str, f64) {
    if let Some(stripped) = time.strip_suffix('Z') {
        return (stripped, 0.0);
    }
    if let Some(pos) = time.rfind(['+', '-']).filter(|p| *p >= 5) {
        let (t, off) = time.split_at(pos);
        let sign = if off.starts_with('-') { -1.0 } else { 1.0 };
        let mut parts = off[1..].split(':');
        let h: f64 = parts.next().and_then(|v| v.get(..2)).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let m: f64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        return (t, sign * (h * 3600.0 + m * 60.0));
    }
    (time, 0.0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn since_phrase(snapshot: &Snapshot) -> String {
    match counter_days(snapshot) {
        Some(days) if days >= 2.0 => format!("in the {} days since the server last restarted", days.round()),
        Some(days) => format!("in the {} hours since the server last restarted", (days * 24.0).round().max(1.0)),
        None => "since the server last restarted".to_string(),
    }
}

/// "a read from Orders", "an UPDATE of Users" — a short handle for a query in titles.
pub fn query_handle(q: &QueryStat) -> String {
    if let Some(object) = &q.object_name {
        return format!("a statement in {object}");
    }
    let text = q.text.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    let lower = text.to_ascii_lowercase();
    let verb = ["select", "insert", "update", "delete", "merge", "with"]
        .into_iter()
        .find(|v| lower.starts_with(v))
        .unwrap_or("");
    let table = main_table(text, &lower, verb);
    match (verb, table) {
        ("select" | "with", Some(t)) => format!("a read from {t}"),
        ("insert", Some(t)) => format!("an insert into {t}"),
        ("update", Some(t)) => format!("an update of {t}"),
        ("delete", Some(t)) => format!("a delete from {t}"),
        ("merge", Some(t)) => format!("a merge into {t}"),
        _ => "a query".to_string(),
    }
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
    match &snapshot.workload {
        Some(w) if w.window_hours >= 20.0 => format!("about {} times a day", format::count(w.per_day(executions))),
        Some(w) => format!("{} times in {}", format::count(executions), span(w.window_hours)),
        None => format!("{} times", format::count(executions)),
    }
}

/// "the last 7 days" for windowed history, "the 35 minutes since statistics were reset" for cumulative counters.
pub fn window_phrase(workload: &culprit_core::Workload) -> String {
    if workload.windowed {
        return format!("the last {}", span(workload.window_hours));
    }
    format!("the {} since statistics were reset", span(workload.window_hours))
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

pub fn per_day(snapshot: &Snapshot, value: f64) -> f64 {
    snapshot.workload.as_ref().map(|w| w.per_day(value)).unwrap_or(value)
}

pub fn lower_set(items: &[String]) -> Vec<String> {
    let mut v: Vec<String> = items.iter().map(|s| s.to_ascii_lowercase()).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

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
