//! Statement shape and object inspection: table/alias references, CTE write targets, pagination, and DDL/hint checks.

use crate::Engine;
use crate::sql::lexer::{Token, lex, modes, readings};
use crate::sql::safety::{mssql_second_statement, without_parameter_list};

/// Words that end a table reference or can't be one.
const NOT_NAMES: &[&str] = &[
    "as",
    "cross",
    "except",
    "fetch",
    "for",
    "full",
    "group",
    "having",
    "inner",
    "intersect",
    "join",
    "lateral",
    "left",
    "limit",
    "natural",
    "offset",
    "on",
    "only",
    "option",
    "order",
    "outer",
    "partition",
    "returning",
    "right",
    "select",
    "set",
    "straight_join",
    "tablesample",
    "union",
    "using",
    "values",
    "when",
    "where",
    "window",
    "with",
];

/// Tables a statement reads or writes, as written (`schema.table` or `table`), lower-cased and unquoted. Best effort, for
/// tying changes (a dropped index, fresh statistics) to the queries they touch; never used to decide what is safe.
pub fn tables_in(text: &str, engine: Engine) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in table_refs(text, engine) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The aliases a statement gives its tables (`FROM orders AS o`: `o` → `orders`), lower-cased, for plans that name a
/// table by its alias (SQLite's `SCAN o`). Best effort, like [`tables_in`].
pub fn table_aliases(text: &str, engine: Engine) -> Vec<(String, String)> {
    table_refs(text, engine).into_iter().filter_map(|(name, alias)| Some((alias?, name))).collect()
}

/// Whether T-SQL explicitly asks to compile a statement on every run with `OPTION (RECOMPILE)`, including a list
/// with other query hints. Literals, comments and quoted names cannot supply the hint; unreadable text is unknown.
pub fn has_mssql_recompile_hint(text: &str) -> bool {
    let Ok(readings) = readings(text, Engine::Mssql) else { return false };
    readings.iter().all(|tokens| {
        tokens.iter().enumerate().any(|(i, t)| {
            if !matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("option"))
                || tokens.get(i + 1) != Some(&Token::Punct('('))
            {
                return false;
            }
            let mut depth = 1;
            for at in i + 2..tokens.len() {
                if depth == 1
                    && matches!(tokens[at], Token::Word(w) if w.eq_ignore_ascii_case("recompile"))
                    && matches!(tokens[at - 1], Token::Punct('(' | ','))
                    && matches!(tokens.get(at + 1), Some(Token::Punct(',' | ')')))
                {
                    return true;
                }
                match tokens[at] {
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            false
        })
    })
}

/// The table whose optimizer statistics T-SQL refreshes. Comments and literals cannot name the operation, and
/// `UPDATE statistics SET …` (including an alias) is an ordinary row update. Ambiguous quoted text stays unknown.
pub fn mssql_statistics_target(text: &str) -> Option<String> {
    let readings = readings(without_parameter_list(text), Engine::Mssql).ok()?;
    let target = |tokens: &[Token<'_>]| -> Option<String> {
        if !matches!(tokens.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("update"))
            || !matches!(tokens.get(1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("statistics"))
            || tokens.iter().any(|t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("set")))
        {
            return None;
        }
        let mut at = 2;
        loop {
            let name = match tokens.get(at)? {
                Token::Word(w) if !w.starts_with('@') => (*w).to_string(),
                Token::Quoted(w, b']') => w.replace("]]", "]"),
                Token::Quoted(w, b'"') => w.replace("\"\"", "\""),
                _ => return None,
            };
            if tokens.get(at + 1) != Some(&Token::Punct('.')) {
                return (!name.is_empty()).then_some(name);
            }
            at += 2;
        }
    };
    let first = target(readings.first()?)?;
    readings.iter().all(|tokens| target(tokens).as_deref() == Some(first.as_str())).then_some(first)
}

/// Whether suggested SQL creates an index. Identifiers, literals and comments cannot supply the operation.
pub fn creates_index(text: &str, engine: Engine) -> bool {
    readings(text, engine)
        .is_ok_and(|readings| readings.iter().all(|tokens| (0..tokens.len()).any(|at| index_creation(&tokens[at..]))))
}

fn index_creation(tokens: &[Token<'_>]) -> bool {
    if !matches!(tokens.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("create")) {
        return false;
    }
    for token in &tokens[1..] {
        match token {
            Token::Word(w) if w.eq_ignore_ascii_case("index") => return true,
            Token::Word(w)
                if ["unique", "clustered", "nonclustered", "columnstore", "spatial", "fulltext", "bitmap"]
                    .iter()
                    .any(|kind| w.eq_ignore_ascii_case(kind)) => {}
            _ => return false,
        }
    }
    false
}

/// A creation script whose indexes include every row. Whitespace, comments and quoted identifiers do not turn a
/// filtered index into one that can enforce a foreign-key lookup for every parent row.
pub fn creates_unfiltered_index(text: &str, engine: Engine) -> bool {
    creates_index(text, engine)
        && readings(text, engine).is_ok_and(|readings| {
            !readings.is_empty()
                && readings.iter().all(|tokens| {
                    tokens.split(|t| matches!(t, Token::Semicolon(_))).filter(|statement| !statement.is_empty()).all(
                        |statement| {
                            index_creation(statement)
                                && (engine != Engine::Mssql || mssql_second_statement(statement).is_none())
                                && !statement
                                    .iter()
                                    .any(|t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("where")))
                        },
                    )
                })
        })
}

/// Whether an UPDATE's only assignment and returned value are this counter. This establishes only its SQL shape;
/// application requirements (per-parent numbering, gaps, ordering and atomicity) still need to be checked.
pub fn isolated_counter_update(text: &str, engine: Engine, column: &str) -> bool {
    let Ok(readings) = readings(without_parameter_list(text), engine) else { return false };
    readings.iter().all(|tokens| {
        if !matches!(tokens.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("update")) {
            return false;
        }
        let Some(set) = tokens.iter().position(|t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("set"))) else {
            return false;
        };
        let Some(eq) = tokens[set + 1..].iter().position(|t| *t == Token::Punct('=')) else { return false };
        let lhs = &tokens[set + 1..set + 1 + eq];
        if !matches!(lhs.last(), Some(Token::Word(w) | Token::Quoted(w, _)) if w.eq_ignore_ascii_case(column)) {
            return false;
        }
        let mut depth = 0;
        let mut end = tokens.len();
        for (i, t) in tokens.iter().enumerate().skip(set + 1) {
            match t {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => depth -= 1,
                Token::Punct(',') if depth == 0 => return false,
                Token::Word(w)
                    if depth == 0
                        && ["output", "returning", "where", "from"].iter().any(|k| w.eq_ignore_ascii_case(k)) =>
                {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let Some(at) = tokens.iter().position(
            |t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("output") || w.eq_ignore_ascii_case("returning")),
        ) else {
            return engine == Engine::Mysql
                && tokens[set + 1..end]
                    .iter()
                    .any(|t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("last_insert_id")));
        };
        let tail = &tokens[at + 1..];
        let stop = tail
            .iter()
            .position(|t| {
                matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("where") || w.eq_ignore_ascii_case("from"))
                    || matches!(t, Token::Semicolon(_))
            })
            .unwrap_or(tail.len());
        let names: Vec<_> = tail[..stop]
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) | Token::Quoted(w, _) => Some(*w),
                Token::Punct('.') => None,
                _ => Some(""),
            })
            .collect();
        match names.as_slice() {
            [c] => c.eq_ignore_ascii_case(column),
            [table, c] => !table.eq_ignore_ascii_case("deleted") && c.eq_ignore_ascii_case(column),
            _ => false,
        }
    })
}

/// The base table of a write through a simple CTE, where every lexical reading agrees. Joins, unions, nested
/// sources and otherwise ambiguous CTEs stay unresolved; a display must not invent a table for them.
pub fn cte_write_target(text: &str, engine: Engine) -> Option<String> {
    fn name(tokens: &[Token<'_>], at: &mut usize) -> Option<String> {
        let mut parts = Vec::new();
        loop {
            let part = match tokens.get(*at)? {
                Token::Word(w) | Token::Quoted(w, _) if !w.starts_with('@') => *w,
                _ => return None,
            };
            // The display target is a dotted string, so a dot inside a quoted part cannot be represented without
            // ambiguity. Keep that uncommon shape unresolved instead of naming the wrong base table.
            if part.contains('.') || part.contains([']', '"', '`']) {
                return None;
            }
            parts.push(part.to_string());
            *at += 1;
            if tokens.get(*at) != Some(&Token::Punct('.')) {
                break;
            }
            *at += 1;
        }
        Some(parts.join("."))
    }
    fn read(tokens: &[Token<'_>]) -> Option<String> {
        let tokens = &tokens[tokens.iter().take_while(|t| matches!(t, Token::Semicolon(_))).count()..];
        let word = |i: usize, k: &str| matches!(tokens.get(i), Some(Token::Word(w)) if w.eq_ignore_ascii_case(k));
        if !word(0, "with") {
            return None;
        }
        let mut at = if word(1, "recursive") { 2 } else { 1 };
        let mut ctes: Vec<(String, Option<String>)> = Vec::new();
        loop {
            let cte = name(tokens, &mut at)?;
            if !word(at, "as") {
                return None;
            }
            at += 1;
            if word(at, "not") {
                at += 1;
            }
            if word(at, "materialized") {
                at += 1;
            }
            if tokens.get(at) != Some(&Token::Punct('(')) {
                return None;
            }
            at += 1;
            let start = at;
            let mut depth = 1;
            while let Some(t) = tokens.get(at) {
                match t {
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                at += 1;
            }
            if depth != 0 {
                return None;
            }
            let body = &tokens[start..at];
            let sources: Vec<_> = body
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    matches!(
                        t,
                        Token::Word(w)
                            if ["from", "join", "union", "intersect", "except"]
                                .iter()
                                .any(|k| w.eq_ignore_ascii_case(k))
                    )
                })
                .collect();
            let base = if let [(i, Token::Word(w))] = sources.as_slice() {
                if w.eq_ignore_ascii_case("from") {
                    let mut from = i + 1;
                    let n = name(body, &mut from)?;
                    let mut depth = 0;
                    let ambiguous = body[from..]
                        .iter()
                        .take_while(|t| {
                            !matches!(
                                t,
                                Token::Word(w)
                                    if ["where", "order", "group", "limit", "offset"]
                                        .iter()
                                        .any(|k| w.eq_ignore_ascii_case(k))
                            )
                        })
                        .any(|t| {
                            match t {
                                Token::Punct('(') => depth += 1,
                                Token::Punct(')') => depth -= 1,
                                Token::Punct(',') if depth == 0 => return true,
                                _ => {}
                            }
                            false
                        });
                    if matches!(body.get(from), Some(Token::Punct(',' | '('))) || ambiguous {
                        None
                    } else if let Some((_, base)) = ctes.iter().find(|(c, _)| c.eq_ignore_ascii_case(&n)) {
                        base.clone()
                    } else if n.eq_ignore_ascii_case(&cte) {
                        None
                    } else {
                        Some(n)
                    }
                } else {
                    None
                }
            } else {
                None
            };
            ctes.push((cte, base));
            at += 1;
            if tokens.get(at) != Some(&Token::Punct(',')) {
                break;
            }
            at += 1;
        }
        if !word(at, "update") && !word(at, "delete") {
            return None;
        }
        at += 1;
        if word(at, "from") {
            at += 1;
        }
        let target = name(tokens, &mut at)?;
        ctes.into_iter().find(|(c, _)| c.eq_ignore_ascii_case(&target))?.1
    }
    let readings = readings(without_parameter_list(text), engine).ok()?;
    let first = read(readings.first()?)?;
    readings.iter().all(|t| read(t).as_deref() == Some(first.as_str())).then_some(first)
}

/// Resolve display targets centrally for every provider, including saved reports and injected snapshots.
pub fn resolve_write_targets(snapshot: &mut crate::Snapshot) {
    let engine = snapshot.server.engine;
    if let Some(w) = &mut snapshot.workload {
        for q in &mut w.queries {
            q.resolved_write_target = cte_write_target(&q.text, engine);
        }
    }
}

fn table_refs(text: &str, engine: Engine) -> Vec<(String, Option<String>)> {
    let Some(tokens) = modes(engine).iter().find_map(|m| lex(text, engine, *m).ok()) else {
        return Vec::new();
    };
    let word = |i: usize| match tokens.get(i) {
        Some(Token::Word(w)) => Some((*w, false)),
        Some(Token::Quoted(w, _)) => Some((*w, true)),
        _ => None,
    };
    let keyword = |i: usize, list: &[&str]| matches!(tokens.get(i), Some(Token::Word(w)) if list.iter().any(|k| w.eq_ignore_ascii_case(k)));
    // A dotted name starting at `i`: its last two parts, and where it ends.
    let name_at = |mut i: usize| -> Option<(String, usize)> {
        let mut parts = Vec::new();
        loop {
            let (w, quoted) = word(i)?;
            if !quoted && (w.starts_with('@') || NOT_NAMES.iter().any(|k| w.eq_ignore_ascii_case(k))) {
                return None;
            }
            parts.push(w.to_ascii_lowercase());
            i += 1;
            if tokens.get(i) == Some(&Token::Punct('.')) && word(i + 1).is_some() {
                i += 1;
            } else {
                break;
            }
        }
        let keep = parts.len().saturating_sub(2);
        Some((parts[keep..].join("."), i))
    };
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let into = keyword(i, &["into", "update", "table", "insert", "delete", "merge"]);
        if !into && !keyword(i, &["from", "join", "using"]) {
            i += 1;
            continue;
        }
        i += 1;
        // T-SQL's `INSERT t`, `DELETE t` and `MERGE t` may leave out INTO and FROM.
        if keyword(i, &["into", "from"]) {
            i += 1;
        }
        while keyword(i, &["only", "lateral"]) {
            i += 1;
        }
        while let Some((name, next)) = name_at(i) {
            // `FROM generate_series(…)` is a function; `INSERT INTO t (a, b)` lists columns.
            if !into && tokens.get(next) == Some(&Token::Punct('(')) {
                i = next;
                break;
            }
            i = next;
            if keyword(i, &["as"]) {
                i += 1;
            }
            let mut alias = None;
            if let Some((a, quoted)) = word(i)
                && (quoted || !NOT_NAMES.iter().any(|k| a.eq_ignore_ascii_case(k)))
            {
                alias = Some(a.to_ascii_lowercase());
                i += 1;
            }
            out.push((name, alias));
            if tokens.get(i) == Some(&Token::Punct(',')) {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// The most rows a LIMIT count is read as: past it the number isn't a page size, and a count taken from a real
/// statement is kept out of redaction only as a statement's shape.
const MAX_LIMIT_ROWS: u64 = 1_000_000_000;

/// The text right after the statement's own LIMIT: the last one outside strings, comments and parentheses (a
/// subquery's LIMIT isn't the statement's), the same in every reading of the text (see [`readings`]). `None` without
/// one, or for text the lexer can't read.
fn after_limit(text: &str, engine: Engine) -> Option<&str> {
    let mut at = None;
    for tokens in readings(text, engine).ok()? {
        let mut depth = 0i32;
        let mut last = None;
        for t in &tokens {
            match t {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => depth -= 1,
                Token::Word(w) if depth == 0 && w.eq_ignore_ascii_case("limit") => {
                    last = Some(w.as_ptr() as usize - text.as_ptr() as usize + w.len());
                }
                _ => {}
            }
        }
        if at.is_some_and(|a| Some(a) != last) {
            return None;
        }
        at = last;
    }
    text.get(at?..)
}

/// Whether the statement has a LIMIT of its own, whatever its count.
pub fn has_limit(text: &str, engine: Engine) -> bool {
    after_limit(text, engine).is_some()
}

/// A literal top-level pagination offset, agreed by every SQL lexing mode. Parameter values and expressions remain
/// unknown. Keywords in comments, strings, quoted names and nested subqueries cannot produce an offset.
pub fn offset_count(text: &str, engine: Engine) -> Option<f64> {
    fn literal(rest: &str) -> Option<f64> {
        let rest = rest.trim_start();
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n = rest.get(..digits)?.parse::<u64>().ok()?;
        let tail = rest.get(digits..)?.trim_start();
        if !tail.is_empty()
            && !tail.starts_with([',', ';', ')'])
            && !["row", "rows", "fetch"].iter().any(|word| {
                tail.get(..word.len()).is_some_and(|s| s.eq_ignore_ascii_case(word))
                    && tail.get(word.len()..).is_some_and(|s| s.chars().next().is_none_or(char::is_whitespace))
            })
        {
            return None;
        }
        (n <= 1_000_000_000).then_some(n as f64)
    }
    let mut agreed = None;
    for tokens in readings(text, engine).ok()? {
        let mut depth = 0;
        let mut value = None;
        for token in tokens {
            match token {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => depth -= 1,
                Token::Word(w) if depth == 0 && w.eq_ignore_ascii_case("offset") => {
                    let end = w.as_ptr() as usize - text.as_ptr() as usize + w.len();
                    value = literal(text.get(end..)?);
                }
                _ => {}
            }
        }
        if value.is_none()
            && matches!(engine, Engine::Mysql | Engine::Sqlite)
            && let Some(rest) = after_limit(text, engine)
        {
            let rest = rest.trim_start();
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if rest.get(digits..).is_some_and(|r| r.trim_start().starts_with(',')) {
                value = literal(rest);
            }
        }
        if let Some(previous) = agreed {
            if previous != value {
                return None;
            }
        } else {
            agreed = Some(value);
        }
    }
    agreed.flatten()
}

/// The row count the statement's own LIMIT asks for, where its text writes it right after the keyword (`LIMIT 50`,
/// MySQL's `LIMIT 10, 50`, `LIMIT 50 OFFSET 10`); `None` without a LIMIT, for a placeholder (`LIMIT ?`, `LIMIT $1`), for
/// anything else between the keyword and the count, and for a count past [`MAX_LIMIT_ROWS`].
pub fn limit_count(text: &str, engine: Engine) -> Option<f64> {
    let rest = after_limit(text, engine)?;
    let number = |s: &str| -> Option<(u64, usize)> {
        let s = s.trim_start_matches([' ', '\t', '\n', '\r']);
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        let n = s.get(..digits)?.parse::<u64>().ok()?;
        Some((n, s.len() - digits))
    };
    let (first, left) = number(rest)?;
    let after_first = &rest[rest.len() - left..];
    // `LIMIT offset, count`: the count is the second number.
    let (count, left) = match after_first.trim_start().strip_prefix(',') {
        Some(second) => number(second)?,
        None => (first, left),
    };
    let tail = &rest[rest.len() - left..];
    let ends = tail.chars().next().is_none_or(|c| c.is_ascii_whitespace() || matches!(c, ';' | ')'));
    (ends && (1..=MAX_LIMIT_ROWS).contains(&count)).then_some(count as f64)
}

/// Extract an application call-site annotation from SQL comments (Marginalia, SqlCommenter, Rails Query Logs, or
/// `path/to/file.ext:line` comments). Returns `None` when the statement carries no call-site comment.
pub fn extract_call_site(sql: &str) -> Option<String> {
    let mut comments = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                        continue;
                    }
                    if bytes[i] == q {
                        i += 1;
                        if i < bytes.len() && bytes[i] == q {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'$' => {
                // Skip PostgreSQL dollar-quoted strings ($tag$...$tag$ or $$...$$)
                let tag_start = i;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'$' {
                    i += 1;
                    let tag = &bytes[tag_start..i];
                    while i + tag.len() <= bytes.len() {
                        if &bytes[i..i + tag.len()] == tag {
                            i += tag.len();
                            break;
                        }
                        i += 1;
                    }
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let start = i + 2;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if let Some(body) = sql.get(start..i) {
                    comments.push(body.trim());
                }
                i = (i + 2).min(bytes.len());
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                let start = i + 2;
                i += 2;
                while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                    i += 1;
                }
                if let Some(body) = sql.get(start..i) {
                    comments.push(body.trim());
                }
            }
            _ => i += 1,
        }
    }
    for comment in comments {
        if let Some(site) = parse_comment_call_site(comment) {
            return Some(site);
        }
    }
    None
}

fn parse_comment_call_site(comment: &str) -> Option<String> {
    let mut file = None;
    let mut line = None;
    let mut func = None;
    let mut controller = None;
    let mut action = None;
    let mut route = None;
    let mut job = None;

    for part in split_comment_kv(comment) {
        let Some((raw_k, raw_v)) = part.split_once('=').or_else(|| {
            let (k, v) = part.split_once(':')?;
            // Avoid treating bare `path/to/file.rb:42` as key=`path/to/file.rb`, val=`42`
            (!has_source_extension(k.trim())).then_some((k, v))
        }) else {
            continue;
        };
        let k = raw_k.trim().trim_start_matches(['*', '@']).to_ascii_lowercase();
        let v = unquote_comment_val(raw_v.trim());
        if v.is_empty() {
            continue;
        }
        match k.as_str() {
            "file" | "filepath" | "source" => file = Some(v),
            "line" | "lineno" => line = Some(v),
            "function" | "func" | "method" => func = Some(v),
            "controller" => controller = Some(v),
            "action" => action = Some(v),
            "route" => route = Some(v),
            "job" => job = Some(v),
            _ => {}
        }
    }

    let mut location = match (file, line) {
        (Some(f), Some(l)) if !f.contains(':') && l.chars().all(|c| c.is_ascii_digit()) => Some(format!("{f}:{l}")),
        (Some(f), _) if is_file_line_token(&f) => Some(f),
        (_, Some(l)) if is_file_line_token(&l) => Some(l),
        (Some(f), _) if has_source_extension(&f) => Some(f),
        _ => None,
    };

    if location.is_none() {
        for word in comment.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '\'' | '"' | '`'));
            if is_file_line_token(trimmed) {
                location = Some(trimmed.to_string());
                break;
            }
        }
    }

    let mut context = Vec::new();
    if let Some(f) = func {
        context.push(format!("function={f}"));
    }
    if let Some(r) = route {
        context.push(format!("route={r}"));
    }
    if let Some(c) = controller {
        context.push(format!("controller={c}"));
    }
    if let Some(a) = action {
        context.push(format!("action={a}"));
    }
    if let Some(j) = job {
        context.push(format!("job={j}"));
    }

    match (location, context.is_empty()) {
        (Some(loc), true) => Some(loc),
        (Some(loc), false) => Some(format!("{loc} ({})", context.join(", "))),
        (None, false) => Some(context.join(", ")),
        (None, true) => None,
    }
}

fn split_comment_kv(comment: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = comment.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        match (in_quote, bytes[i]) {
            (Some(q), b) if b == q => in_quote = None,
            (None, b'\'' | b'"') => in_quote = Some(bytes[i]),
            (None, b',' | b'\n' | b'\r' | b';') => {
                if let Some(slice) = comment.get(start..i) {
                    let trimmed = slice.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed);
                    }
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(slice) = comment.get(start..) {
        let trimmed = slice.trim();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
    }
    out
}

fn unquote_comment_val(val: &str) -> String {
    let val = val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(val);
    let val = val.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(val);
    // Url-encoded `%2F` / `%3A` in OpenTelemetry SqlCommenter values; replace backticks with single quotes so markdown code spans never break.
    val.replace("%2F", "/").replace("%2f", "/").replace("%3A", ":").replace("%3a", ":").replace('`', "'")
}

fn has_source_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//") {
        return false;
    }
    let file_part = path.split(':').next().unwrap_or(path);
    let Some((stem, ext)) = file_part.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "py"
            | "rb"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "go"
            | "java"
            | "kt"
            | "cs"
            | "php"
            | "ex"
            | "exs"
            | "scala"
            | "sql"
            | "c"
            | "cc"
            | "cpp"
            | "h"
    )
}

fn is_file_line_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//") {
        return false;
    }
    let Some((path, rest)) = token.split_once(':') else {
        return false;
    };
    if path.is_empty() || !has_source_extension(path) {
        return false;
    }
    let line_part = rest.split(':').next().unwrap_or(rest);
    !line_part.is_empty() && line_part.chars().all(|c| c.is_ascii_digit())
}
