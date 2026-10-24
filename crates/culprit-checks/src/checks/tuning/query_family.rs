//! The `query-family` check: The same statement sent many times with different literal values (or value-list lengths), so the database compiles and tracks each copy separately; and plan caches filled with single-use plans.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

pub fn query_family_info() -> CheckInfo {
    CheckInfo {
        id: "query-family".into(),
        title: "Statements with literals baked in".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Yugabyte],
        requires: vec![],
        description:
            "The same statement sent many times with different literal values (or value-list lengths), so the \
            database compiles and tracks each copy separately; and plan caches filled with single-use plans."
                .into(),
    }
}

check!(QueryFamilyCheck: query_family_info, query_family, &[Evidence::Query(QueryMeasure::Work)]);

pub fn query_family(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let mut out = Vec::new();
    let threshold = if engine == Engine::Mssql { 20.0 } else { 5.0 };
    let total_ms = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
    for family in snapshot.query_families.iter().filter(|f| f.variants >= threshold) {
        // What the copies differ in decides the fix; copies whose text is the same are split by something no
        // parameterization changes (the role that ran them, the schema a name resolved in, the module they're in).
        let Some(split) = family_split(snapshot, family) else { continue };
        let mut f = finding(
            "query-family",
            &family.key.chars().take(40).collect::<String>(),
            Category::Queries,
            subject(SubjectKind::Query, family.key.clone()),
        );
        let handle = statement_handle(without_declaration(&family.sample));
        let (title, why) = match (&split, engine) {
            (FamilySplit::Declarations(examples), _) => (
                format!(
                    "The app sends {} versions of {} that differ only in how their parameters are declared",
                    format::count(family.variants),
                    handle
                ),
                format!(
                    "because their parameters are declared differently from call to call ({}): drivers do this when a \
                     string parameter's size comes from each value (ADO.NET's AddWithValue, for one). SQL Server \
                     compiles and keeps a plan for each declaration",
                    match examples.as_slice() {
                        [one] => format!("this one declares `{one}`"),
                        _ => examples.iter().map(|e| format!("`{e}`")).collect::<Vec<_>>().join(" and "),
                    }
                ),
            ),
            (_, Engine::Mssql) => (
                format!(
                    "The app sends {} versions of {} that differ only in literal values",
                    format::count(family.variants),
                    handle
                ),
                "so SQL Server compiles each one separately and keeps a plan for each".to_string(),
            ),
            _ => (
                format!("{} is tracked as {} separate statements", capitalize(&handle), format::count(family.variants)),
                "because its value lists (IN lists, multi-row VALUES) have different lengths; each length is planned \
                 and counted on its own"
                    .to_string(),
            ),
        };
        f.title = title;
        f.summary = format!(
            "{} variants of the same statement ran {} times{}, {}{}. Statistics split across the copies too, so none \
             of them looks as heavy as the statement really is.",
            format::count(family.variants),
            format::count(family.executions),
            if total_ms > 0.0 {
                format!(" for {} of database time", format::percent((family.total_time_ms / total_ms).clamp(0.0, 1.0)))
            } else {
                String::new()
            },
            why,
            family
                .cached_bytes
                .filter(|b| *b >= 1024.0 * 1024.0)
                .map(|b| format!(" ({} of plan cache)", format::bytes(b)))
                .unwrap_or_default()
        );
        let share = if total_ms > 0.0 { family.total_time_ms / total_ms } else { 0.0 };
        // Thousands of copies cost compiles and plan cache whatever their time; a share needs an amount behind it.
        let timed = share >= 0.1 && severity_for_time(snapshot, family.total_time_ms, share) >= Severity::Medium;
        f.severity = if family.variants >= 1000.0 || timed { Severity::Medium } else { Severity::Low };
        f.impact.score = base_score(f.severity) + family.variants.log10().max(0.0) * 3.0 + share * 20.0;
        f.impact.label = format!("{} copies", format::count(family.variants));
        f.impact.db_time_share = (share > 0.0).then_some(share);
        f.confidence = Confidence::High;
        f.metrics.push(Metric::new("Variants", family.variants, Unit::Count));
        f.metrics.push(Metric::new("Executions", family.executions, Unit::Count));
        if let Some(b) = family.cached_bytes {
            f.metrics.push(Metric::new("Plan cache", b, Unit::Bytes));
        }
        f.queries = family
            .query_ids
            .iter()
            .filter(|id| snapshot.workload.as_ref().is_some_and(|w| w.query(id).is_some()))
            .map(|id| QueryRef { id: id.clone(), plan_node: None, note: None })
            .collect();
        // The change is in the code that sends the statement: the caveats quote one copy of it, as recorded.
        let in_the_code = |change: &str| {
            if family.sample.is_empty() {
                capitalize(change)
            } else {
                format!("In the code that sends this statement (one copy is quoted below, as recorded), {change}")
            }
        };
        f.fix = Some(match (&split, engine) {
            (FamilySplit::Declarations(_), _) => Fix {
                summary: in_the_code(
                    "declare each string parameter with a fixed size, the column's length, instead of letting the \
                    driver size it from the value (in ADO.NET, set SqlParameter.Size or use Add with a SqlDbType and \
                    size rather than AddWithValue; other drivers have a similar setting). Then one plan serves every \
                    value. Forced parameterization can't help: these statements are already parameterized.",
                ),
                sql: None,
                cost: None,
            },
            (_, Engine::Mssql) => Fix {
                summary: in_the_code(
                    "send values as parameters (sp_executesql, or your driver's parameterized queries) so one plan \
                    serves every value. If you can't change the app, forced parameterization does it for the whole \
                    database.",
                ),
                sql: Some(format!(
                    "-- Prefer fixing the app. As a database-wide fallback:\nALTER DATABASE {} SET PARAMETERIZATION FORCED;",
                    quote(engine, &snapshot.server.database)
                )),
                cost: Some("Forced parameterization can make some queries pick a plan that suits one value badly (parameter sensitivity); watch the regression findings after switching.".into()),
            },
            _ => Fix {
                summary: in_the_code(
                    "pass value lists as one array parameter (`= ANY($1)` in Postgres) instead of a growing IN list, \
                    and batch multi-row inserts with UNNEST or COPY.",
                ),
                sql: None,
                cost: None,
            },
        });
        if matches!(&split, FamilySplit::Unknown) || matches!(&split, FamilySplit::Declarations(e) if e.len() < 2) {
            f.confidence = Confidence::Medium;
            f.caveats.push(
                "Culprit sees one of these statements' text, so what the others differ in is inferred from it.".into(),
            );
        }
        // pg_stat_statements keeps an entry for each copy until it is evicted or reset: a deploy stops new ones, and
        // the old ones stay counted.
        f.verify = vec![if engine.sql_syntax() == culprit_core::SqlSyntax::Postgres {
            format!(
                "After a deploy, no new copies should appear: pg_stat_statements keeps an entry for each copy until it evicts it or its statistics are reset, so compare the family's copies in a scan taken when the change ships and one a day or more later ({} in this scan): the count should stay as it is, or fall.",
                format::count(family.variants)
            )
        } else {
            "Re-scan after a deploy: this family shrinks to one or a few statements.".into()
        }];
        if !family.sample.is_empty() {
            f.caveats.push(format!("One of them, as recorded:\n{}", family.sample.trim()));
        }
        out.push(f);
    }

    if let Some(cache) = &snapshot.plan_cache
        && cache.total_bytes > 0.0
    {
        let share = cache.single_use_bytes / cache.total_bytes;
        if cache.single_use_bytes >= 256.0 * 1024.0 * 1024.0 && share >= 0.25 {
            let ad_hoc_on = snapshot.setting("optimize for ad hoc workloads").is_some_and(|s| s.value.trim() == "1");
            let mut f = finding(
                "query-family",
                "plan-cache",
                Category::Configuration,
                subject(SubjectKind::Setting, "plan cache"),
            );
            f.title = format!(
                "Plans used once fill {} of the plan cache ({})",
                format::percent(share),
                format::bytes(cache.single_use_bytes)
            );
            f.summary = format!(
                "{} plans in the cache were compiled for a statement that ran exactly once. They take memory that could \
                 hold data pages and reusable plans, and every one cost a compilation.{}",
                format::count(cache.single_use_plans),
                if ad_hoc_on {
                    ""
                } else {
                    " 'Optimize for ad hoc workloads' is off, so each keeps a full plan instead of a small stub."
                }
            );
            f.severity = if share >= 0.5 { Severity::Medium } else { Severity::Low };
            f.impact.score = base_score(f.severity) + share * 10.0;
            f.impact.label = format!("{} single-use", format::bytes(cache.single_use_bytes));
            f.confidence = Confidence::High;
            f.metrics.push(Metric::new("Single-use plans", cache.single_use_plans, Unit::Count));
            f.metrics.push(Metric::new("Their memory", cache.single_use_bytes, Unit::Bytes));
            f.metrics.push(Metric::new("Share of the cache", share, Unit::Ratio));
            // Where the setting lives: per database on Azure SQL Database, a server option elsewhere (the DB parameter
            // group on RDS, which refuses sp_configure).
            let (how, sql) = if ad_hoc_on {
                (String::new(), None)
            } else if snapshot.server.on(culprit_core::Platform::AzureSqlDatabase) {
                (
                    String::new(),
                    Some("ALTER DATABASE SCOPED CONFIGURATION SET OPTIMIZE_FOR_AD_HOC_WORKLOADS = ON;".to_string()),
                )
            } else {
                let change = crate::config::server_option(&snapshot.server, "optimize for ad hoc workloads", "1");
                (if change.sql.is_none() { format!(" {}", change.how) } else { String::new() }, change.sql)
            };
            f.fix = Some(Fix {
                summary: if ad_hoc_on {
                    "Parameterize the statements behind it (see the families above).".into()
                } else {
                    format!("Keep only a stub for a plan's first use, and parameterize the statements behind it.{how}")
                },
                sql,
                cost: None,
            });
            f.verify = vec!["Re-scan after a day: single-use plans take a much smaller share of the cache.".into()];
            out.push(f);
        }
    }
    out
}

/// What the copies of a statement family differ in, as far as their texts show.
#[derive(Debug, PartialEq)]
pub(super) enum FamilySplit {
    /// Literal values (SQL Server) or value-list lengths (PostgreSQL): what parameterizing fixes.
    Values,
    /// The same parameterized statement with its parameters declared differently (sizes taken from each value).
    Declarations(Vec<String>),
    /// Inferred from the one text Culprit has.
    Unknown,
}

/// None when the copies are the same text: the split is by who ran them (PostgreSQL keeps an entry per role), the
/// schema a name resolved in, the module they belong to or the connection's settings, which no change to how values
/// are sent fixes.
pub(super) fn family_split(snapshot: &Snapshot, family: &culprit_core::QueryFamily) -> Option<FamilySplit> {
    let members: Vec<&QueryStat> = snapshot
        .workload
        .as_ref()
        .map(|w| family.query_ids.iter().filter_map(|id| w.query(id)).collect())
        .unwrap_or_default();
    if members.len() >= 2 {
        let first = normalized(&members[0].text);
        if members.iter().any(|q| normalized(&q.text) != first) {
            return Some(FamilySplit::Values);
        }
        let mut declarations: Vec<String> = Vec::new();
        for d in members.iter().filter_map(|q| q.parameters.as_deref()) {
            if !declarations.iter().any(|x| x == d) {
                declarations.push(d.to_string());
            }
        }
        return (declarations.len() >= 2)
            .then(|| FamilySplit::Declarations(declarations.into_iter().take(2).collect()));
    }
    match snapshot.server.engine {
        Engine::Mssql => match split_declaration(&family.sample) {
            // Parameters the application named (`@p0`, `@P1`, `@id`) and no literal left in the text: the copies can't
            // differ in values, so the declarations are what differs.
            (Some(declaration), body) if !auto_parameterized(declaration) && !has_literals(body) => {
                Some(FamilySplit::Declarations(vec![declaration.trim().to_string()]))
            }
            _ => Some(FamilySplit::Unknown),
        },
        // Entries are grouped with their value lists collapsed: without a list, the copies are the same text.
        _ => has_value_list(&family.sample).then_some(FamilySplit::Values),
    }
}

/// `(@p0 nvarchar(7),@p1 int)SELECT …` as (declaration, statement), the way Query Store keeps a parameterized text.
pub(super) fn split_declaration(text: &str) -> (Option<&str>, &str) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('(') || !trimmed[1..].trim_start().starts_with('@') {
        return (None, text);
    }
    let mut depth = 0usize;
    for (i, c) in trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (Some(&trimmed[1..i]), trimmed[i + 1..].trim_start());
                }
            }
            _ => {}
        }
    }
    (None, text)
}

pub(super) fn without_declaration(text: &str) -> &str {
    split_declaration(text).1
}

/// SQL Server's own parameterization names parameters by number (`@1`, `@2`): the application sent literals.
pub(super) fn auto_parameterized(declaration: &str) -> bool {
    declaration
        .split(',')
        .any(|d| d.trim().strip_prefix('@').is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit())))
}

/// A string or number literal in a statement (outside names, parameters and `TOP (n)`).
pub(super) fn has_literals(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            return true;
        }
        if c == '[' {
            while i < chars.len() && chars[i] != ']' {
                i += 1;
            }
        } else if c.is_ascii_digit() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let in_name = prev.is_some_and(|p| p.is_ascii_alphanumeric() || matches!(p, '_' | '@' | '#' | '$'));
            let before: String = chars[..i].iter().collect();
            let top = before.trim_end().trim_end_matches('(').trim_end().ends_with("top");
            if !in_name && !top {
                return true;
            }
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    false
}

/// An IN list, multi-row VALUES or ARRAY of parameters: what PostgreSQL entries of different lengths come from.
pub(super) fn has_value_list(text: &str) -> bool {
    let t = text.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ").replace("( ", "(");
    [" in ($", " in($", "values ($", "values($", "array[$", "any(array[$"].iter().any(|m| t.contains(m))
}

/// Statement text without comments and with whitespace collapsed, to compare two copies.
pub(super) fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r);
        } else if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r);
        } else {
            let c = rest.chars().next().expect("non-empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
