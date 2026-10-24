use culprit_core::{Engine, QueryStat, Snapshot};

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
    if let Some(base) = culprit_core::sql::cte_write_target(text, engine) {
        return Some(base.to_ascii_lowercase());
    }
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
    q.plan.as_ref().map_or_else(|| is_monitoring_text(&q.text), |plan| is_monitoring_statement(&q.text, plan.engine))
}

/// Native YSQL size functions can record their internal, unqualified catalog reads without the caller's query tag.
/// Resolve relation tokens rather than treating a catalog name in a string or an application column as monitoring.
fn is_monitoring_statement(text: &str, engine: Engine) -> bool {
    is_monitoring_text(text)
        || (engine.sql_syntax() == culprit_core::SqlSyntax::Postgres
            // tables_in folds identifiers; quoted, case-sensitive application names stay unclassified here.
            && !text.contains('"')
            && text.to_ascii_lowercase().contains("pg_")
            && culprit_core::sql::tables_in(text, engine).iter().any(|table| {
                matches!(
                    table.as_str(),
                    "pg_class"
                        | "pg_attribute"
                        | "pg_namespace"
                        | "pg_index"
                        | "pg_constraint"
                        | "pg_type"
                        | "pg_proc"
                        | "pg_database"
                        | "pg_settings"
                        | "pg_roles"
                        | "pg_extension"
                )
            }))
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
    snapshot.queries().iter().filter(|q| !is_monitoring_statement(&q.text, snapshot.server.engine))
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

/// "a read from Orders", "an UPDATE of Users" — a short handle for a query in titles.
pub fn query_handle(q: &QueryStat) -> String {
    if let Some(object) = &q.object_name {
        return format!("a statement in {object}");
    }
    if let Some(base) = &q.resolved_write_target {
        let verb = first_verb(main_statement(&q.text)).map(|(_, v)| v);
        let name = base.rsplit('.').next().unwrap_or(base);
        if verb == Some("update") {
            return format!("an update of {name}");
        }
        if verb == Some("delete") {
            return format!("a delete from {name}");
        }
    }
    let body = statement_body(&q.text);
    let main = main_statement(body);
    if body.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("with")) && main.len() != body.len() {
        match first_verb(main).map(|(_, v)| v) {
            Some("update") => return "an update in a WITH statement".into(),
            Some("delete") => return "a delete in a WITH statement".into(),
            _ => {}
        }
    }
    statement_handle(&q.text)
}

/// The statement a `WITH` list leads to: `WITH b AS (SELECT …) DELETE FROM jobs …` is a delete, not a read. The text
/// from the first verb outside the common table expressions' parentheses and strings; any other text as it is.
fn statement_body(text: &str) -> &str {
    let mut text = text.trim_start();
    loop {
        let unwrapped = culprit_core::sql::without_parameter_list(text).trim_start();
        if unwrapped.len() != text.len() {
            text = unwrapped;
            continue;
        }
        if let Some(rest) = text.strip_prefix("--") {
            text = rest.split_once('\n').map_or("", |(_, rest)| rest).trim_start();
        } else if let Some(rest) = text.strip_prefix("/*") {
            let bytes = rest.as_bytes();
            let (mut at, mut depth) = (0, 1);
            while at + 1 < bytes.len() && depth > 0 {
                if &bytes[at..at + 2] == b"/*" {
                    depth += 1;
                    at += 2;
                } else if &bytes[at..at + 2] == b"*/" {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
            if depth != 0 {
                return text;
            }
            text = rest[at..].trim_start();
        } else if let Some(rest) = text.strip_prefix(';') {
            text = rest.trim_start();
        } else {
            break;
        }
    }
    text
}

pub fn main_statement(text: &str) -> &str {
    let text = statement_body(text).trim_start_matches(|c: char| c.is_whitespace() || c == '(');
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
