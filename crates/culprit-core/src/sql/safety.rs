//! Statement boundary, read-only classification, T-SQL marker injection, and session-statement detection.

use crate::sql::lexer::{Token, readings};
use crate::{Engine, Error, Result, sql::StatementCheck};

fn too_many() -> Error {
    Error::invalid("This text holds more than one statement")
        .with_hint("Run or explain one statement at a time: select it in the editor first.")
}

/// The one statement in `text`, without trailing semicolons or trailing comments. Errors when the text holds more than
/// one statement, an executable comment, or quoting this can't follow — under every server setting that changes lexing.
pub fn single_statement(text: &str, engine: Engine) -> Result<&str> {
    let mut cut: Option<usize> = None;
    for (n, tokens) in readings(text, engine).map_err(Error::invalid)?.into_iter().enumerate() {
        let first_semicolon = tokens.iter().position(|t| matches!(t, Token::Semicolon(_)));
        let end = match first_semicolon {
            Some(at) => {
                if tokens[at + 1..].iter().any(|t| !matches!(t, Token::Semicolon(_))) {
                    return Err(too_many());
                }
                let Token::Semicolon(offset) = tokens[at] else { unreachable!() };
                offset
            }
            None => text.len(),
        };
        if n > 0 && cut != Some(end) {
            return Err(too_many());
        }
        cut = Some(end);
    }
    let statement = text[..cut.unwrap_or(text.len())].trim();
    let readings = readings(statement, engine).map_err(Error::invalid)?;
    if statement.is_empty() || readings.iter().any(|tokens| tokens.is_empty()) {
        return Err(Error::invalid("There is no statement to run"));
    }
    // T-SQL runs consecutive statements without a semicolon between them: `SELECT 1 COMMIT CREATE TABLE …` is three.
    if engine == Engine::Mssql
        && let Some(word) = readings.iter().find_map(|tokens| mssql_second_statement(tokens))
    {
        return Err(too_many().with_hint(format!(
            "SQL Server reads `{word}` here as the start of another statement. Run or explain one statement at a time."
        )));
    }
    Ok(statement)
}

/// Keywords that can only start a T-SQL statement: at the top level after the first word they begin a second one.
pub(crate) const MSSQL_STATEMENTS: &[&str] = &[
    "EXEC",
    "EXECUTE",
    "COMMIT",
    "ROLLBACK",
    "BEGIN",
    "SAVE",
    "GRANT",
    "REVOKE",
    "DENY",
    "DBCC",
    "KILL",
    "SHUTDOWN",
    "BACKUP",
    "RESTORE",
    "USE",
    "DECLARE",
    "WAITFOR",
    "RAISERROR",
    "THROW",
    "PRINT",
    "GO",
    "CHECKPOINT",
    "RECONFIGURE",
    "DEALLOCATE",
    "WHILE",
    "RETURN",
    "GOTO",
    "TRUNCATE",
    "CREATE",
    "ALTER",
    "DROP",
    "IF",
    "SET",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "WITH",
    "OPEN",
    "CLOSE",
    "BULK",
    "SETUSER",
    "REVERT",
    "ENABLE",
    "DISABLE",
    "RECEIVE",
    "SEND",
    "MOVE",
    "GET",
    "UPDATETEXT",
    "WRITETEXT",
    "READTEXT",
    "FETCH",
    "BREAK",
    "CONTINUE",
    "LINENO",
    "END",
    "ADD",
];

/// The keyword that starts a second statement in one T-SQL text, if any. Conservative: a word SQL Server might read
/// as a new statement counts unless the first statement's grammar clearly allows it there (UNION SELECT, a CTE's main
/// query, MERGE's THEN UPDATE, ALTER TABLE … DROP COLUMN, DROP … IF EXISTS, a table hint's WITH (…)).
pub(crate) fn mssql_second_statement<'a>(tokens: &[Token<'a>]) -> Option<&'a str> {
    second_statement_at(tokens, 0)
}

/// IFs nested deeper than this count as more than one statement: the check recurses once per IF, and text from an
/// editor or an agent can nest them without end.
const MAX_IF_NESTING: u8 = 8;

fn second_statement_at<'a>(tokens: &[Token<'a>], nesting: u8) -> Option<&'a str> {
    // Query Store's parameterized text starts with the parameter declarations: `(@P1 nvarchar(4000))SELECT …`.
    let tokens = match tokens {
        [Token::Punct('('), Token::Word(w), ..] if w.starts_with('@') => {
            let mut depth = 0i32;
            let close = tokens.iter().position(|t| {
                match t {
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => depth -= 1,
                    _ => {}
                }
                depth == 0
            });
            match close {
                Some(at) => &tokens[at + 1..],
                None => tokens,
            }
        }
        _ => tokens,
    };
    let upper = |t: &Token| match t {
        Token::Word(w) => Some(w.to_ascii_uppercase()),
        _ => None,
    };
    let first = tokens.iter().find_map(upper)?;
    let words: Vec<String> = tokens.iter().filter_map(upper).collect();
    // A module body (procedure, function, trigger) is a batch of its own: Culprit never runs one as a read, and a
    // production change asks first with the full text shown.
    let module = |w: &str| matches!(w, "PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER");
    if (first == "CREATE" || first == "ALTER") && words.iter().take(4).any(|w| module(w)) {
        return None;
    }
    // The main query a statement may still introduce: a CTE's or INSERT's SELECT (or a CTE's DML), one each.
    let mut main_query = matches!(first.as_str(), "WITH" | "INSERT");
    // The statement a CTE introduces (`WITH c AS (…) UPDATE …`): its clauses are read as that statement's.
    let mut cte_main: Option<String> = None;
    // An UPDATE (a statement's, or a MERGE's `THEN UPDATE`) whose SET clause hasn't come yet. That SET is
    // the UPDATE's; any other SET starts a statement (`UPDATE t SET x = 1 SET NOCOUNT ON` is two).
    let mut update_awaits_set = first == "UPDATE";
    let mut depth = 0i32;
    let mut seen_first = false;
    let mut previous: Option<String> = None;
    let mut seen_select = false;
    // GRANT/REVOKE/DENY name permissions (SELECT, EXECUTE, ALTER…) before ON or TO: words, not statements.
    let mut permissions = matches!(first.as_str(), "GRANT" | "REVOKE" | "DENY");
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth -= 1,
            _ => {}
        }
        let Token::Word(raw) = token else {
            if !matches!(token, Token::Punct(_)) {
                previous = None;
            }
            continue;
        };
        let word = raw.to_ascii_uppercase();
        if permissions {
            if matches!(word.as_str(), "ON" | "TO" | "FROM") {
                permissions = false;
            }
            previous = Some(word);
            seen_first = true;
            continue;
        }
        if !seen_first {
            seen_first = true;
            previous = Some(word);
            continue;
        }
        // `t.exec`, `[t].exec`, `@begin`, `#tmp` and the like are names. A number before the dot is not: SQL Server reads
        // `1.EXEC` as the literal `1.` and then a new statement, and `1e.EXEC` likewise.
        let digit = |t: Option<&Token>| matches!(t, Some(Token::Punct(c)) if c.is_ascii_digit());
        let qualified = i >= 2
            && matches!(tokens[i - 1], Token::Punct('.'))
            && match tokens[i - 2] {
                Token::Quoted(..) => true,
                Token::Word(w) => {
                    w.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '#' || c == '@')
                        && !digit(i.checked_sub(3).and_then(|k| tokens.get(k)))
                }
                _ => false,
            };
        let next_word = tokens.get(i + 1).and_then(upper);
        let next_is_paren = matches!(tokens.get(i + 1), Some(Token::Punct('(')));
        let prev = previous.clone().unwrap_or_default();
        let starts = depth <= 0
            && !qualified
            && !raw.starts_with(['@', '#'])
            && MSSQL_STATEMENTS.contains(&word.as_str())
            && match word.as_str() {
                "SELECT" => {
                    let set_op = matches!(prev.as_str(), "UNION" | "EXCEPT" | "INTERSECT" | "ALL");
                    let allowed = set_op || (main_query && !seen_select);
                    if allowed && !set_op {
                        main_query = false;
                        if first == "WITH" {
                            cte_main = Some(word.clone());
                        }
                    }
                    !allowed
                }
                "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
                    let merge_action = (first == "MERGE" || cte_main.as_deref() == Some("MERGE")) && prev == "THEN";
                    let join_hint = word == "MERGE" && next_word.as_deref() == Some("JOIN");
                    let cte_dml = first == "WITH" && main_query && !seen_select;
                    if cte_dml {
                        main_query = false;
                        cte_main = Some(word.clone());
                    }
                    if word == "UPDATE" && (merge_action || cte_dml) {
                        update_awaits_set = true;
                    }
                    !(merge_action || join_hint || cte_dml)
                }
                // Table hints and options: WITH (NOLOCK), WITH TIES, WITH CHECK, WITH GRANT OPTION…; a CTE is `WITH x AS (`.
                "WITH" => {
                    matches!(tokens.get(i + 1), Some(Token::Word(_)) | Some(Token::Quoted(..)))
                        && tokens.get(i + 2).and_then(upper).as_deref() == Some("AS")
                }
                // UPDATE … SET is a clause, after a CTE too (`WITH c AS (…) UPDATE c SET …`).
                "SET" if update_awaits_set => {
                    update_awaits_set = false;
                    false
                }
                "SET" => {
                    let statement =
                        if first == "WITH" { cte_main.as_deref().unwrap_or("WITH") } else { first.as_str() };
                    matches!(
                        statement,
                        "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "DROP" | "CREATE" | "TRUNCATE"
                    )
                }
                "DROP" => first != "ALTER",
                "ALTER" => !(first == "CREATE" && prev == "OR"),
                "IF" => !(prev == "DROP" || matches!(next_word.as_deref(), Some("EXISTS") | Some("NOT"))),
                "ROLLBACK" | "GRANT" | "REVOKE" | "DENY" => prev != "WITH",
                "EXEC" | "EXECUTE" => !(first == "INSERT" && !seen_select),
                // `OPEN`/`CLOSE`/`GET`/`MOVE` and friends are only statements when followed by a name, not a call.
                "OPEN" | "CLOSE" | "GET" | "MOVE" | "SEND" | "RECEIVE" | "ENABLE" | "DISABLE" | "BULK" => {
                    !next_is_paren && first != "ALTER"
                }
                // A cursor fetch; in a query it's `OFFSET n ROWS FETCH NEXT m ROWS ONLY`.
                "FETCH" => !matches!(prev.as_str(), "ROW" | "ROWS"),
                // CASE … END is an expression; END CONVERSATION is a statement.
                "END" => next_word.as_deref() == Some("CONVERSATION"),
                // ALTER TABLE … ADD is a clause; ADD SIGNATURE / COUNTERSIGNATURE is a statement.
                "ADD" => matches!(next_word.as_deref(), Some("SIGNATURE") | Some("COUNTERSIGNATURE")),
                _ => true,
            };
        if word == "SELECT" && depth <= 0 {
            seen_select = true;
        }
        // `IF <condition> <statement>` is one statement: the first one after the condition is its body, checked as a
        // statement of its own. An ELSE branch or a BEGIN … END block of several still counts as more than one.
        if starts && first == "IF" {
            if nesting >= MAX_IF_NESTING {
                return Some(raw);
            }
            return second_statement_at(&tokens[i..], nesting + 1);
        }
        if starts {
            return Some(raw);
        }
        previous = Some(word);
    }
    None
}

/// Statements a read starts with, per engine.
fn read_starts(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Mssql => &["SELECT", "WITH"],
        Engine::Postgres => &["SELECT", "WITH", "TABLE", "VALUES", "SHOW", "EXPLAIN"],
        Engine::Mysql => &["SELECT", "WITH", "TABLE", "VALUES", "SHOW", "EXPLAIN", "DESCRIBE", "DESC"],
        // PRAGMA can write (`PRAGMA journal_mode = WAL`), so it isn't a read.
        Engine::Sqlite => &["SELECT", "WITH", "VALUES", "EXPLAIN"],
    }
}

/// Words that change data or state wherever they appear in a read-shaped statement: data-modifying CTEs, SELECT INTO,
/// EXPLAIN ANALYZE (which runs the statement).
const WRITES: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "INTO", "ANALYZE", "ANALYSE", "TRUNCATE",
    // SQL Server table hints that take update or exclusive locks.
    "UPDLOCK", "XLOCK", "TABLOCKX",
];

/// Built-in functions with side effects (or that run SQL from a string) that a read-only transaction doesn't stop.
fn side_effect_functions(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Postgres => &[
            "pg_terminate_backend",
            "pg_cancel_backend",
            "set_config",
            "pg_reload_conf",
            "pg_rotate_logfile",
            "pg_switch_wal",
            "pg_create_restore_point",
            "pg_promote",
            "pg_logical_emit_message",
            "pg_notify",
            "pg_read_file",
            "pg_read_binary_file",
            "pg_ls_dir",
            "pg_file_write",
            "nextval",
            "setval",
            "query_to_xml",
            "query_to_xml_and_xmlschema",
            "query_to_xmlschema",
            "dblink",
            "dblink_exec",
            "lo_import",
            "lo_export",
            "lo_unlink",
            "lo_create",
            "lo_from_bytea",
            "lo_put",
        ],
        Engine::Mysql => {
            &["get_lock", "release_lock", "release_all_locks", "load_file", "master_pos_wait", "source_pos_wait"]
        }
        Engine::Mssql => &["openrowset", "opendatasource", "openquery"],
        // Loading native code, and the file functions of the CLI and the fileio extension.
        Engine::Sqlite => &["load_extension", "readfile", "writefile", "edit", "fts3_tokenizer"],
    }
}

/// Families of built-ins with side effects that a read-only transaction doesn't stop, matched by name pattern so new
/// members (`pg_stat_reset_*`, `pg_create_*_replication_slot`) are covered too.
fn side_effect_family(engine: Engine, name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    match engine {
        Engine::Sqlite => false,
        Engine::Postgres => {
            (n.starts_with("pg_") && n.contains("reset"))
                || n.contains("replication_slot")
                || n.starts_with("pg_replication_origin")
                // pg_wal_replay_pause and friends act; pg_wal_lsn_diff only subtracts (replication-lag queries use it).
                || (["pg_backup_", "pg_start_backup", "pg_stop_backup", "pg_wal_", "pg_switch_", "pg_log_"]
                    .iter()
                    .any(|p| n.starts_with(p))
                    && n != "pg_wal_lsn_diff")
                || ["pg_create_", "pg_drop_", "pg_copy_", "pg_file_", "pg_read_", "pg_ls_", "pg_import_"]
                    .iter()
                    .any(|p| n.starts_with(p))
                || n.starts_with("pg_terminate_")
                || n.starts_with("pg_cancel_")
                || n.starts_with("pg_promote")
                // Functions that run SQL passed as a string: anything blocked could hide inside the literal.
                || n.starts_with("crosstab")
                || n.ends_with("_exec")
                || n.starts_with("dblink")
                || n.starts_with("query_to_")
                || n.starts_with("lo_")
        }
        Engine::Mysql => {
            matches!(n.as_str(), "setval" | "nextval" | "lastval" | "sleep" | "benchmark")
                || n.ends_with("_lock")
                || n.starts_with("release_")
        }
        Engine::Mssql => {
            n.starts_with("open") && matches!(n.as_str(), "openrowset" | "opendatasource" | "openquery" | "openxml")
        }
    }
}

/// True when `text` is one statement that only reads: it starts like a read, contains no data-changing clause
/// (data-modifying CTE, SELECT INTO, locking reads, EXPLAIN ANALYZE) and calls no built-in with side effects.
/// User-defined functions can still have side effects, so callers also run reads in a read-only transaction where the
/// engine has one, and a read-only login is the only complete guarantee.
pub fn reads_only(text: &str, engine: Engine) -> bool {
    let Ok(statement) = single_statement(text, engine) else { return false };
    let advisory = |w: &str| engine == Engine::Postgres && w.to_ascii_lowercase().contains("advisory");
    let Ok(readings) = readings(statement, engine) else { return false };
    readings.iter().all(|tokens| {
        let words: Vec<(usize, String)> = tokens
            .iter()
            .enumerate()
            .filter_map(|(i, t)| match t {
                Token::Word(w) => Some((i, w.to_ascii_uppercase())),
                _ => None,
            })
            .collect();
        let Some((_, first)) = words.first() else { return false };
        if !read_starts(engine).contains(&first.as_str()) {
            return false;
        }
        let functions = side_effect_functions(engine);
        let unsafe_call = |name: &str| {
            functions.iter().any(|f| f.eq_ignore_ascii_case(name)) || advisory(name) || side_effect_family(engine, name)
        };
        let keywords_ok = words.iter().enumerate().all(|(n, (at, word))| {
            let previous = n.checked_sub(1).map(|p| words[p].1.as_str());
            let calls = matches!(tokens.get(at + 1), Some(Token::Punct('(')));
            let locking = word == "SHARE" && matches!(previous, Some("FOR") | Some("IN") | Some("KEY"));
            // NEXT VALUE FOR advances a sequence (SQL Server, MariaDB): a write a read-only transaction doesn't undo.
            let sequence = engine != Engine::Postgres && word == "VALUE" && previous == Some("NEXT");
            !(WRITES.contains(&word.as_str()) || locking || sequence || (calls && unsafe_call(word)))
        });
        // A quoted name can call a function too ("pg_terminate_backend"(…), [openquery](…), `get_lock`(…)). Compare its
        // unescaped name; a name the lexer can't decode (escapes, Postgres U&"…") is treated as unsafe.
        let quoted_ok = tokens.iter().enumerate().all(|(at, token)| {
            let Token::Quoted(raw, close) = token else { return true };
            if !matches!(tokens.get(at + 1), Some(Token::Punct('('))) {
                return true;
            }
            let unicode = at >= 2
                && matches!(tokens[at - 1], Token::Punct('&'))
                && matches!(tokens[at - 2], Token::Word(w) if w.eq_ignore_ascii_case("u"));
            if unicode || raw.contains('\\') {
                return false;
            }
            let doubled = String::from_utf8(vec![*close, *close]).unwrap_or_default();
            !unsafe_call(&raw.replace(&doubled, &(*close as char).to_string()))
        });
        keywords_ok && quoted_ok
    })
}

/// Classify `text` for the run path: reads run as they are, anything else needs confirmation on production.
pub fn check(text: &str, engine: Engine) -> StatementCheck {
    let statement = match single_statement(text, engine) {
        Ok(statement) => statement,
        Err(e) => {
            return StatementCheck { reads_only: false, verb: String::new(), warning: None, problem: Some(e.message) };
        }
    };
    let reads = reads_only(statement, engine);
    let words: Vec<String> = readings(statement, engine)
        .ok()
        .and_then(|r| r.into_iter().next())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        })
        .collect();
    let verb = words.first().cloned().unwrap_or_default();
    let has = |w: &str| words.iter().any(|x| x == w);
    let warning = match verb.as_str() {
        _ if reads => None,
        "UPDATE" | "DELETE" if !has("WHERE") => {
            Some("There is no WHERE clause, so this changes every row in the table.".to_string())
        }
        "TRUNCATE" => Some("This removes every row in the table.".to_string()),
        "DROP" => Some("This removes the object and everything in it.".to_string()),
        "ALTER" => Some("This changes the structure; a large table can be locked while it runs.".to_string()),
        "SELECT" | "WITH" | "TABLE" | "VALUES" | "SHOW" | "EXPLAIN" => Some(
            "Culprit can't prove this only reads: it locks rows, writes into a table, or calls a function with side effects."
                .to_string(),
        ),
        _ => None,
    };
    StatementCheck { reads_only: reads, verb, warning, problem: None }
}

/// The keywords after which [`mark_mssql_statements`] puts its marker: one starts every statement Query Store records.
const MARKED_KEYWORDS: [&str; 7] = ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "EXEC", "EXECUTE"];

/// What `CREATE` or `ALTER` defines when its body is kept as text: everything after it is the module's definition.
const MODULES: [&str; 5] = ["PROC", "PROCEDURE", "FUNCTION", "TRIGGER", "VIEW"];

/// T-SQL `text` with ` {marker}` (a comment) after every SELECT, INSERT, UPDATE, DELETE, MERGE, EXEC and EXECUTE
/// keyword, so SQL Server's Query Store, which keeps each statement of a batch with its own text, records every one of
/// them with the marker. Strings, quoted names and comments are left alone, and a comment between two tokens changes
/// nothing the server runs. Unchanged when there is no such keyword, when this lexer can't read the text (the server
/// then sees what was written), and when the text defines a procedure, function, trigger or view: the marker would
/// become part of its body, and every statement it runs would be left out of the workload for good.
pub fn mark_mssql_statements(text: &str, marker: &str) -> String {
    let Ok(readings) = readings(text, Engine::Mssql) else { return text.to_string() };
    let ends = |tokens: &[Token<'_>]| -> Option<Vec<usize>> {
        let words: Vec<&str> =
            tokens.iter().filter_map(|t| if let Token::Word(w) = t { Some(*w) } else { None }).collect();
        let is = |w: Option<&&str>, list: &[&str]| w.is_some_and(|w| list.iter().any(|k| w.eq_ignore_ascii_case(k)));
        let defines_module = words.iter().enumerate().any(|(i, w)| {
            is(Some(w), &["CREATE", "ALTER"])
                && (is(words.get(i + 1), &MODULES) || (is(words.get(i + 1), &["OR"]) && is(words.get(i + 3), &MODULES)))
        });
        if defines_module {
            return None;
        }
        Some(
            words
                .iter()
                .filter(|w| is(Some(w), &MARKED_KEYWORDS))
                .map(|w| (w.as_ptr() as usize - text.as_ptr() as usize, w.len()))
                // `#select` (a temporary table) and `x.update` are names, not statements.
                .filter(|(at, _)| !matches!(at.checked_sub(1).map(|b| text.as_bytes()[b]), Some(b'#' | b'.')))
                .map(|(at, len)| at + len)
                .collect(),
        )
    };
    let mut readings = readings.iter().map(|tokens| ends(tokens));
    let Some(Some(ends)) = readings.next() else { return text.to_string() };
    // Every reading (QUOTED_IDENTIFIER on and off) must put the markers in the same places.
    if readings.any(|other| other.as_ref() != Some(&ends)) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + ends.len() * (marker.len() + 1));
    let mut from = 0;
    for end in ends {
        out.push_str(&text[from..end]);
        out.push(' ');
        out.push_str(marker);
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// The statement past the parameter declarations Query Store keeps in front of a parameterized one
/// (`(@P1 nvarchar(4000))UPDATE …` is an UPDATE); any other text as it is.
pub fn without_parameter_list(text: &str) -> &str {
    let text = text.trim_start();
    let Some(inside) = text.strip_prefix('(').filter(|rest| rest.trim_start().starts_with('@')) else {
        return text;
    };
    let mut depth = 1;
    for (at, c) in inside.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 1 => return &inside[at + 1..],
            ')' => depth -= 1,
            _ => {}
        }
    }
    text
}

/// Statements a driver, a pool or a framework sends around the application's own: transaction control (`BEGIN`,
/// `COMMIT`, `SAVEPOINT x`, `END`), session settings (`SET`, `USE`, `RESET`, `DISCARD ALL`), `SHOW`, `PRAGMA`, and the
/// keep-alive `SELECT 1` a pool checks a connection with. They count in the totals (a slow COMMIT is time the database
/// spent), but they aren't queries to tune or to batch: a loop in the application doesn't send them.
pub fn is_session_statement(text: &str) -> bool {
    let mut rest = text.trim_start();
    // Leading comments: `/* ping */ SELECT 1`, a driver's `-- keep-alive`.
    loop {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let lower = rest.trim_end().trim_end_matches(';').trim_end().to_ascii_lowercase();
    // One statement only: `SET NOCOUNT ON; SELECT …` is the SELECT's batch.
    if lower.contains(';') {
        return false;
    }
    let first = lower.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
    if matches!(
        first,
        "begin"
            | "start"
            | "commit"
            | "rollback"
            | "savepoint"
            | "release"
            | "end"
            | "set"
            | "use"
            | "reset"
            | "discard"
            | "show"
            | "pragma"
    ) {
        // `START` begins a transaction; `SET` also starts `SET TRANSACTION` and session settings alike.
        return first != "start" || lower.starts_with("start transaction");
    }
    // A keep-alive: SELECT of one constant or parameter, nothing read.
    lower.strip_prefix("select").is_some_and(|value| {
        let value = value.trim();
        !value.is_empty()
            && (value.chars().all(|c| c.is_ascii_digit())
                || value == "?"
                || (value.starts_with('$') && value[1..].chars().all(|c| c.is_ascii_digit()))
                || value == "@@version"
                || value == "version()")
    })
}
