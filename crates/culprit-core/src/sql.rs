//! Statement-level SQL lexing for every engine: enough to tell one statement from several and reads from writes, without
//! a parser. Text from a workload or an agent is untrusted, so the rules are conservative: anything this can't lex with
//! confidence is treated as unsafe. It is a guard in front of the engines' own protections (read-only sessions and
//! transactions, prepared statements), never a replacement for them.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{Engine, Error, Result};

/// How backslashes behave inside quoted text. MySQL and Postgres both have a server setting that flips this
/// (NO_BACKSLASH_ESCAPES, standard_conforming_strings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backslash {
    Escapes,
    Literal,
}

/// What `"…"` is. MySQL's ANSI_QUOTES and SQL Server's QUOTED_IDENTIFIER flip it between a string and a name,
/// independently of the backslash setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DoubleQuote {
    String,
    Identifier,
}

/// One combination of the server settings that change how text is lexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    backslash: Backslash,
    double_quote: DoubleQuote,
}

/// Every combination a server can be running with: the text is lexed under each and every reading must agree, so a
/// session setting Culprit didn't choose can never make the server see a statement the lexer didn't.
fn modes(engine: Engine) -> &'static [Mode] {
    use Backslash::*;
    use DoubleQuote::*;
    match engine {
        // QUOTED_IDENTIFIER ON (the driver default) and OFF.
        Engine::Mssql => {
            &[Mode { backslash: Literal, double_quote: Identifier }, Mode { backslash: Literal, double_quote: String }]
        }
        // standard_conforming_strings on and off (E'…' always escapes).
        Engine::Postgres => &[
            Mode { backslash: Escapes, double_quote: Identifier },
            Mode { backslash: Literal, double_quote: Identifier },
        ],
        // No backslash escapes. A double-quoted word is a name, or a string when no such name exists (SQLite's legacy rule).
        Engine::Sqlite => {
            &[Mode { backslash: Literal, double_quote: Identifier }, Mode { backslash: Literal, double_quote: String }]
        }
        // NO_BACKSLASH_ESCAPES × ANSI_QUOTES.
        Engine::Mysql => &[
            Mode { backslash: Escapes, double_quote: String },
            Mode { backslash: Literal, double_quote: String },
            Mode { backslash: Escapes, double_quote: Identifier },
            Mode { backslash: Literal, double_quote: Identifier },
        ],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token<'a> {
    Word(&'a str),
    /// A quoted identifier's raw text between its delimiters (`"name"`, `[name]`, `` `name` ``). The server treats it
    /// like a word (it can name a function), so the read check must see it.
    Quoted(&'a str, u8),
    Semicolon(usize),
    Punct(char),
}

/// Top-level tokens, with literals and comments skipped. Errors on text it can't lex safely.
fn lex(text: &str, engine: Engine, mode: Mode) -> std::result::Result<Vec<Token<'_>>, &'static str> {
    let mut tokens = Vec::new();
    lex_into(text, engine, mode, &mut tokens).map(|()| tokens)
}

/// The message for a quote that never closes: the one lexing error that depends on the server's settings.
const UNCLOSED_QUOTE: &str = "A quoted string or name is never closed";

/// Every reading of `text` under the server settings that change lexing (see [`modes`]). A setting under which the
/// text has a quote that never closes is one where the server rejects the text whole, so that reading is left out — as
/// long as nothing before the open quote could run on its own: MySQL runs multi-statement text a statement at a time, so
/// a `;` before it keeps the error. Every other error (comments, executable comments, invisible characters) refuses the
/// text in any reading, and at least one reading must succeed.
fn readings(text: &str, engine: Engine) -> std::result::Result<Vec<Vec<Token<'_>>>, &'static str> {
    let mut out = Vec::new();
    let mut unclosed = None;
    for &mode in modes(engine) {
        let mut tokens = Vec::new();
        match lex_into(text, engine, mode, &mut tokens) {
            Ok(()) => out.push(tokens),
            Err(e) => {
                let ran_before = engine == Engine::Mysql && tokens.iter().any(|t| matches!(t, Token::Semicolon(_)));
                if e != UNCLOSED_QUOTE || ran_before {
                    return Err(e);
                }
                unclosed = Some(e);
            }
        }
    }
    if out.is_empty() { Err(unclosed.unwrap_or(UNCLOSED_QUOTE)) } else { Ok(out) }
}

fn lex_into<'a>(
    text: &'a str,
    engine: Engine,
    mode: Mode,
    tokens: &mut Vec<Token<'a>>,
) -> std::result::Result<(), &'static str> {
    let backslash = mode.backslash;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c' => i += 1,
            b';' => {
                tokens.push(Token::Semicolon(i));
                i += 1;
            }
            // MySQL only treats `--` as a comment when whitespace or a control character follows; `1--1` is arithmetic.
            b'-' if next == Some(b'-') && (engine != Engine::Mysql || bytes.get(i + 2).is_none_or(|b| *b <= b' ')) => {
                i = line_end(bytes, i);
            }
            b'#' if engine == Engine::Mysql => i = line_end(bytes, i),
            b'/' if next == Some(b'*') => {
                if engine == Engine::Mysql && matches!(bytes.get(i + 2), Some(b'!') | Some(b'M')) {
                    // `/*! … */` and MariaDB's `/*M! … */` are executed by the server, so they are code, not comments.
                    let executable = bytes.get(i + 2) == Some(&b'!') || bytes.get(i + 3) == Some(&b'!');
                    if executable {
                        return Err("Executable comments (/*! … */) aren't allowed here");
                    }
                }
                i = block_comment_end(bytes, i, engine != Engine::Mysql)?;
            }
            b'\'' => i = quoted_end(bytes, i, b'\'', backslash == Backslash::Escapes)?,
            b'"' => {
                // A string (MySQL by default, SQL Server with QUOTED_IDENTIFIER OFF) or a name (ANSI_QUOTES, Postgres,
                // QUOTED_IDENTIFIER ON). Backslashes escape only inside a string, and only where the server says so.
                let string = mode.double_quote == DoubleQuote::String;
                let end = quoted_end(bytes, i, b'"', string && backslash == Backslash::Escapes)?;
                if !string {
                    tokens.push(Token::Quoted(&text[i + 1..end - 1], b'"'));
                }
                i = end;
            }
            b'`' if matches!(engine, Engine::Mysql | Engine::Sqlite) => {
                let end = quoted_end(bytes, i, b'`', false)?;
                tokens.push(Token::Quoted(&text[i + 1..end - 1], b'`'));
                i = end;
            }
            b'[' if matches!(engine, Engine::Mssql | Engine::Sqlite) => {
                let end = quoted_end(bytes, i, b']', false)?;
                tokens.push(Token::Quoted(&text[i + 1..end - 1], b']'));
                i = end;
            }
            b'$' if engine == Engine::Postgres => match dollar_tag(bytes, i) {
                Some(tag) => i = dollar_quoted_end(bytes, i, tag)?,
                None => {
                    tokens.push(Token::Punct('$'));
                    i += 1;
                }
            },
            // Characters servers may read as whitespace (or ignore) but this lexer would take for part of a word or a
            // symbol: a non-breaking space or zero-width space between `1` and `EXEC` must never hide the keyword.
            _ if c < 0x20 || c == 0x7f || (c >= 0x80 && text[i..].chars().next().is_some_and(invisible)) => {
                return Err(
                    "The text has a control character or unusual whitespace outside a string; retype it with plain spaces",
                );
            }
            _ if is_word_start(c) => {
                let start = i;
                while i < bytes.len() && is_word_part(bytes[i]) {
                    i += 1;
                }
                let word = &text[start..i];
                // Inside a word too: `pg_stat_reset<zero-width space>()` must not read as an unknown name here while
                // a server sees the function.
                if word.chars().any(invisible) {
                    return Err(
                        "The text has a control character or unusual whitespace outside a string; retype it with plain spaces",
                    );
                }
                // Postgres E'…' strings always treat backslashes as escapes.
                if engine == Engine::Postgres && word.eq_ignore_ascii_case("e") && bytes.get(i) == Some(&b'\'') {
                    i = quoted_end(bytes, i, b'\'', true)?;
                } else {
                    tokens.push(Token::Word(word));
                }
            }
            _ => {
                // Skip the whole UTF-8 sequence so slicing stays on character boundaries.
                let ch = text[i..].chars().next().unwrap_or('?');
                tokens.push(Token::Punct(ch));
                i += ch.len_utf8();
            }
        }
    }
    Ok(())
}

/// Whitespace, controls and format characters outside ASCII: invisible, and lexed differently by different servers.
fn invisible(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || matches!(ch, '\u{00AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c >= 0x80
}

fn is_word_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'@' || c == b'#' || c >= 0x80
}

/// Where a `--` or `#` comment ends. Servers end it at a carriage return as well as a line feed (Postgres does, and
/// so may others): ending it at either one never hides text the server runs, so `-- note\rEXEC …` shows its EXEC.
fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..].iter().position(|b| matches!(b, b'\n' | b'\r')).map_or(bytes.len(), |p| from + p + 1)
}

/// The index after a `/* … */` comment starting at `from`. Postgres and SQL Server nest these; MySQL doesn't.
fn block_comment_end(bytes: &[u8], from: usize, nests: bool) -> std::result::Result<usize, &'static str> {
    let mut depth = 0usize;
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            if depth == 0 || nests {
                depth += 1;
            }
            i += 2;
        } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Ok(i);
            }
        } else {
            i += 1;
        }
    }
    Err("A /* comment */ is never closed")
}

/// The index after a quoted run opened at `from` with `close` (doubled `close` is an escaped one).
fn quoted_end(
    bytes: &[u8],
    from: usize,
    close: u8,
    backslash_escapes: bool,
) -> std::result::Result<usize, &'static str> {
    let mut i = from + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if backslash_escapes && c == b'\\' {
            i += 2;
        } else if c == close {
            if bytes.get(i + 1) == Some(&close) {
                i += 2;
            } else {
                return Ok(i + 1);
            }
        } else {
            i += 1;
        }
    }
    Err(UNCLOSED_QUOTE)
}

/// A Postgres dollar-quote opener at `from` (`$$` or `$tag$`); `$1` is a parameter, not a quote.
fn dollar_tag(bytes: &[u8], from: usize) -> Option<&[u8]> {
    let rest = &bytes[from + 1..];
    let len = rest.iter().position(|b| *b == b'$')?;
    let tag = &rest[..len];
    let valid = tag.first().is_none_or(|b| b.is_ascii_alphabetic() || *b == b'_' || *b >= 0x80)
        && tag.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80);
    valid.then_some(&bytes[from..from + len + 2])
}

fn dollar_quoted_end(bytes: &[u8], from: usize, tag: &[u8]) -> std::result::Result<usize, &'static str> {
    let body = from + tag.len();
    bytes[body..]
        .windows(tag.len())
        .position(|w| w == tag)
        .map(|p| body + p + tag.len())
        .ok_or("A $$-quoted string is never closed")
}

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
const MSSQL_STATEMENTS: &[&str] = &[
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
fn mssql_second_statement<'a>(tokens: &[Token<'a>]) -> Option<&'a str> {
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

/// What a statement does, for the question asked before it changes data on production.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct StatementCheck {
    /// One statement that only reads: it runs without asking, inside a read-only transaction where the engine has one.
    pub reads_only: bool,
    /// The leading keyword, e.g. `UPDATE`.
    pub verb: String,
    /// Why this one deserves a second look (it touches every row, or removes an object), when it does.
    pub warning: Option<String>,
    /// Why the text can't run as one statement (several statements, an unclosed quote), when it can't.
    pub problem: Option<String>,
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

/// Statements a driver, a pool or a framework sends around the application's own: transaction control (`BEGIN`,
/// `COMMIT`, `SAVEPOINT x`, `END`), session settings (`SET`, `USE`, `RESET`, `DISCARD ALL`), `SHOW`, `PRAGMA`, and the
/// keep-alive `SELECT 1` a pool checks a connection with. They count in the totals (a slow COMMIT is time the database
/// spent), but they aren't queries to tune or to batch: a loop in the application doesn't send them.
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
    if !matches!(tokens.first(),Some(Token::Word(w)) if w.eq_ignore_ascii_case("create")) {
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
                                    .any(|t| matches!(t,Token::Word(w) if w.eq_ignore_ascii_case("where")))
                        },
                    )
                })
        })
}

#[cfg(test)]
mod index_creation_tests {
    use super::*;

    #[test]
    fn foreign_key_coverage_cannot_come_from_a_filtered_or_mixed_script() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
            for sql in [
                "CREATE INDEX ix_orders ON shop.orders (state)",
                "/* WHERE state = 1 */ CREATE INDEX ix_orders ON shop.orders (state)",
            ] {
                assert!(creates_unfiltered_index(sql, engine), "{engine:?}: {sql}");
            }
            for sql in [
                "",
                "-- CREATE INDEX ix_orders ON shop.orders (state)",
                "CREATE INDEX ix_orders ON shop.orders (state)\nWHERE state = 1",
                "CREATE INDEX ix_orders ON shop.orders (state) /* filter */ WHERE state IS NOT NULL",
                "CREATE INDEX ix_orders ON shop.orders (state); CREATE INDEX ix_other ON shop.orders (region) WHERE region IS NOT NULL",
                "CREATE INDEX ix_orders ON shop.orders (state); SELECT 1",
                "CREATE INDEX ix_orders ON shop.orders (state); CREATE TABLE other (id int)",
            ] {
                assert!(!creates_unfiltered_index(sql, engine), "{engine:?}: {sql}");
            }
        }
        assert!(creates_unfiltered_index("CREATE INDEX [where] ON [shop].[orders] ([where])", Engine::Mssql));
        assert!(creates_unfiltered_index("CREATE INDEX \"where\" ON shop.orders (\"where\")", Engine::Postgres));
        assert!(creates_unfiltered_index("CREATE INDEX `where` ON shop.orders (`where`)", Engine::Mysql));
    }
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
            let sources: Vec<_> = body.iter().enumerate().filter(|(_, t)| matches!(t, Token::Word(w) if ["from", "join", "union", "intersect", "except"].iter().any(|k| w.eq_ignore_ascii_case(k)))).collect();
            let base = if let [(i, Token::Word(w))] = sources.as_slice() {
                if w.eq_ignore_ascii_case("from") {
                    let mut from = i + 1;
                    let n = name(body, &mut from)?;
                    let mut depth = 0;
                    let ambiguous = body[from..].iter().take_while(|t| {
                        !matches!(t, Token::Word(w) if ["where", "order", "group", "limit", "offset"].iter().any(|k| w.eq_ignore_ascii_case(k)))
                    }).any(|t| {
                        match t {
                            Token::Punct('(') => depth += 1,
                            Token::Punct(')') => depth -= 1,
                            Token::Punct(',') if depth == 0 => return true,
                            _ => {},
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

#[cfg(test)]
mod offset_tests {
    use super::*;
    #[test]
    fn literal_offsets_do_not_accept_expressions_identifiers_or_nested_queries() {
        for engine in Engine::ALL {
            assert_eq!(
                offset_count("SELECT * FROM t OFFSET 12000 ROWS FETCH NEXT 10 ROWS ONLY", engine),
                Some(12000.0)
            );
            for sql in [
                "SELECT 'OFFSET 12000' FROM t",
                "SELECT * FROM (SELECT * FROM t OFFSET 12000) q",
                "SELECT * FROM t OFFSET 12000 + 1",
                "SELECT * FROM t OFFSET :offset",
                "SELECT * FROM t /* OFFSET 12000 */",
                "SELECT * FROM t OFFSET 12000.5",
            ] {
                assert_eq!(offset_count(sql, engine), None, "{engine}: {sql}");
            }
        }
        assert_eq!(offset_count("SELECT * FROM t LIMIT 12000,20", Engine::Mysql), Some(12000.0));
        assert_eq!(offset_count("SELECT * FROM t LIMIT 12000,20", Engine::Sqlite), Some(12000.0));
    }
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

#[cfg(test)]
mod tests {

    #[test]
    fn statistics_maintenance_is_code_not_a_table_name_or_comment() {
        for (text, table) in [
            ("UPDATE STATISTICS dbo.Orders WITH FULLSCAN", "Orders"),
            ("/* before */ UPDATE /* nested /* why */ */ STATISTICS [dbo].[Order Lines]", "Order Lines"),
            ("(@P1 int) UPDATE STATISTICS dbo.Orders", "Orders"),
            ("UPDATE STATISTICS [dbo].[set] WITH FULLSCAN", "set"),
            ("UPDATE STATISTICS [dbo].[Order]]Lines]", "Order]Lines"),
        ] {
            assert_eq!(mssql_statistics_target(text).as_deref(), Some(table), "{text}");
        }
        for text in [
            "-- UPDATE STATISTICS dbo.Orders\nSELECT * FROM dbo.Orders",
            "SELECT 'UPDATE STATISTICS dbo.Orders'",
            "UPDATE statistics SET value = 1",
            "UPDATE statistics s SET value = 1",
            "UPDATE statistics AS s SET value = 1",
            "UPDATE statistics_log SET value = 1",
            "UPDATE STATISTICS",
            "UPDATE STATISTICS dbo.",
            "UPDATE STATISTICS dbo.Orders /* unclosed",
            "UPDATE STATISTICS \"dbo\".\"Orders\"",
        ] {
            assert_eq!(mssql_statistics_target(text), None, "{text}");
        }
    }

    #[test]
    fn a_recompile_hint_is_code_in_the_option_list() {
        for text in [
            "SELECT * FROM dbo.orders WHERE id = @P1 OPTION (RECOMPILE)",
            "select * from dbo.orders option (maxdop 1, /* why */ recompile, force order);",
            "SELECT * FROM dbo.orders OPTION (USE HINT ('DISABLE_PARAMETER_SNIFFING'), RECOMPILE)",
        ] {
            assert!(has_mssql_recompile_hint(text), "{text}");
        }
        for text in [
            "SELECT 'OPTION (RECOMPILE)' FROM dbo.orders",
            "SELECT \"OPTION (RECOMPILE)\", [RECOMPILE] FROM dbo.orders",
            "SELECT * FROM dbo.orders /* OPTION (RECOMPILE) */ OPTION (MAXDOP 1)",
            "SELECT * FROM dbo.orders -- OPTION (RECOMPILE)",
            "SELECT * FROM dbo.orders OPTION (USE HINT ('RECOMPILE'))",
            "SELECT * FROM dbo.orders OPTION (OPTIMIZE FOR (@RECOMPILE = 1))",
            "SELECT * FROM dbo.orders OPTION (RECOMPILE) /* unclosed",
        ] {
            assert!(!has_mssql_recompile_hint(text), "{text}");
        }
    }

    #[test]
    fn a_limit_says_its_count_where_the_text_writes_it() {
        let m = Engine::Mysql;
        assert_eq!(limit_count("SELECT * FROM orders WHERE promo_code = 'X' LIMIT 50", m), Some(50.0));
        assert_eq!(limit_count("SELECT * FROM orders LIMIT 10, 50", m), Some(50.0));
        assert_eq!(limit_count("select * from orders limit 50 offset 10", Engine::Postgres), Some(50.0));
        assert_eq!(limit_count("SELECT * FROM `orders` WHERE `promo_code` = ? LIMIT ?", m), None);
        assert!(has_limit("SELECT * FROM `orders` WHERE `promo_code` = ? LIMIT ?", m));
        assert_eq!(limit_count("SELECT rate_limit FROM plans", m), None);
        assert!(!has_limit("SELECT rate_limit, limit_x FROM plans", m));
        // A LIMIT inside a string or a comment, or a subquery's, isn't the statement's: a number written there is data.
        assert_eq!(limit_count("SELECT * FROM notes WHERE body = 'LIMIT 4242'", m), None);
        assert_eq!(limit_count("SELECT * FROM notes LIMIT 5 /* LIMIT 4242 */", m), Some(5.0));
        assert_eq!(limit_count("SELECT * FROM notes LIMIT 5 -- x, 4242", m), Some(5.0));
        assert_eq!(limit_count("SELECT * FROM t WHERE id IN (SELECT id FROM u LIMIT 7)", m), None);
        assert_eq!(limit_count("SELECT * FROM notes LIMIT /* 4242 */ 5", m), None, "only a count right after it");
        assert_eq!(limit_count("SELECT * FROM notes LIMIT 99999999999", m), None, "not a page size");
        assert_eq!(limit_count("SELECT * FROM notes LIMIT 'x", m), None, "text the lexer can't read");
    }

    #[test]
    fn session_statements_are_the_drivers_not_the_applications() {
        for text in [
            "BEGIN",
            "COMMIT;",
            "START TRANSACTION",
            "SAVEPOINT active_record_1",
            "SET autocommit = ?",
            "SET NAMES utf8mb4",
            "SHOW WARNINGS",
            "DISCARD ALL",
            "/* ping */ SELECT 1",
            "SELECT ?",
            "select $1",
            "PRAGMA foreign_keys = ?",
        ] {
            assert!(is_session_statement(text), "{text}");
        }
        for text in [
            "SELECT 1 FROM orders WHERE id = ?",
            "SELECT id FROM users",
            "SET NOCOUNT ON; SELECT * FROM t",
            "STARTUP",
            "UPDATE t SET a = 1",
            "SELECT count(*)",
        ] {
            assert!(!is_session_statement(text), "{text}");
        }
    }

    #[test]
    fn a_cte_update_and_its_set_clause_are_one_statement() {
        let one = |text: &str| single_statement(text, Engine::Mssql).is_ok();
        // A queue claim: take rows other sessions aren't holding, mark them, return them.
        assert!(one("WITH due AS (SELECT TOP (50) id, next_attempt_at FROM q WITH (UPDLOCK, READPAST, ROWLOCK) \
             WHERE status = 'pending' ORDER BY next_attempt_at) \
             UPDATE due SET next_attempt_at = SYSUTCDATETIME() OUTPUT inserted.id"));
        assert!(one("WITH c AS (SELECT id FROM t) MERGE t USING c ON t.id = c.id WHEN MATCHED THEN UPDATE SET x = 1;"));
        assert!(one("MERGE t USING s ON t.id = s.id WHEN MATCHED AND s.gone = 1 THEN UPDATE SET t.x = 0 \
             WHEN MATCHED THEN UPDATE SET t.x = s.x;"));
        // Only the SET of an UPDATE is a clause: any other SET is another statement.
        assert!(!one("WITH c AS (SELECT 1 AS a) SELECT a FROM c SET NOCOUNT ON"));
        assert!(!one("WITH c AS (SELECT 1 AS a) UPDATE t SET x = 1 SET NOCOUNT ON"));
        assert!(!one("UPDATE t SET x = 1 SET NOCOUNT ON"));
        assert!(one("UPDATE t SET x = (SELECT 1), y = 2 WHERE id = 3"));
    }

    #[test]
    fn a_tsql_if_and_its_body_are_one_statement() {
        let one = |text: &str| single_statement(text, Engine::Mssql).is_ok();
        assert!(one(
            "IF NOT EXISTS (SELECT 1 FROM sys.database_principals WHERE name = 'reader') CREATE USER [reader] FROM EXTERNAL PROVIDER;"
        ));
        assert!(one("IF OBJECT_ID('dbo.t') IS NULL CREATE TABLE dbo.t (id int)"));
        assert!(one("IF EXISTS (SELECT 1 FROM a) INSERT INTO b SELECT * FROM a"));
        assert!(!one("IF 1 = 1 SELECT 1 SELECT 2"));
        assert!(!one("IF 1 = 1 BEGIN SELECT 1 SELECT 2 END"));
        assert!(!one("IF 1 = 1 SELECT 1 ELSE DELETE FROM t"));
        // Nesting is capped: a deep chain is refused at once instead of recursing without end.
        assert!(one(&format!("{}SELECT 1", "IF 1 = 1 ".repeat(8))));
        assert!(!one(&format!("{}SELECT 1", "IF 1 = 1 ".repeat(9))));
        assert!(!one(&format!("{}SELECT 1", "IF 1 = 1 ".repeat(200_000))));
    }

    use super::*;

    #[test]
    fn tables_a_statement_touches() {
        assert_eq!(
            tables_in(
                "SELECT * FROM public.orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = $1",
                Engine::Postgres
            ),
            vec!["public.orders", "customers"]
        );
        assert_eq!(
            tables_in("select a from [dbo].[Order Lines] l, dbo.Items AS i with (nolock)", Engine::Mssql),
            vec!["dbo.order lines", "dbo.items"]
        );
        assert_eq!(
            tables_in("INSERT INTO shop.events (a, b) SELECT a, b FROM `staging`.`events`", Engine::Mysql),
            vec!["shop.events", "staging.events"]
        );
        assert_eq!(
            tables_in("SELECT * FROM generate_series(1, 3) g JOIN LATERAL (SELECT 1) x ON true", Engine::Postgres),
            Vec::<String>::new()
        );
        assert_eq!(tables_in("UPDATE ONLY accounts SET n = n + 1 WHERE id = $1", Engine::Postgres), vec!["accounts"]);
        assert_eq!(tables_in("SELECT 1 FROM db.dbo.t", Engine::Mssql), vec!["dbo.t"]);
        assert_eq!(tables_in("INSERT dbo.hot (v) VALUES (@i)", Engine::Mssql), vec!["dbo.hot"]);
        assert_eq!(tables_in("DELETE FROM jobs WHERE id = $1", Engine::Postgres), vec!["jobs"]);
        assert!(tables_in("SELECT 'unterminated FROM x", Engine::Postgres).is_empty());
    }

    const ALL: [Engine; 3] = [Engine::Mssql, Engine::Postgres, Engine::Mysql];

    #[test]
    fn one_statement_with_trailing_noise_is_fine() {
        for engine in ALL {
            assert_eq!(single_statement("  SELECT 1 ;; ", engine).unwrap(), "SELECT 1");
            assert_eq!(single_statement("SELECT 1; -- done\n", engine).unwrap(), "SELECT 1");
            assert_eq!(single_statement("SELECT 'a;b' AS x", engine).unwrap(), "SELECT 'a;b' AS x");
            assert_eq!(single_statement("SELECT 1 /* a ; b */ + 1", engine).unwrap(), "SELECT 1 /* a ; b */ + 1");
            assert!(single_statement("  ; ", engine).is_err());
            assert!(single_statement("-- only a comment", engine).is_err());
        }
    }

    #[test]
    fn tsql_statements_without_semicolons_are_refused() {
        // SQL Server runs these as several statements: none may pass as one read.
        for text in [
            "SELECT 1 AS a COMMIT CREATE TABLE tempdb.dbo.x (a int)",
            "SELECT 1 EXEC sp_who",
            "SELECT 1 EXECUTE('DROP TABLE t')",
            "SELECT name FROM sys.tables SELECT 2",
            "SELECT 1 DECLARE @x int",
            "SELECT 1 SET NOCOUNT ON",
            "SELECT 1\nGO\nDROP TABLE t",
            "SELECT 1 DBCC FREEPROCCACHE",
            "SELECT 1 KILL 53",
            "SELECT 1 USE master",
            "SELECT 1 WAITFOR DELAY '00:00:10'",
            "SELECT a FROM t DELETE FROM t",
            "SELECT 1 BEGIN TRAN",
            "SELECT 1 IF 1 = 1 DROP TABLE t",
            "WITH x AS (SELECT 1 AS a) SELECT a FROM x SELECT 2",
            "SELECT 1 GRANT SELECT ON t TO public",
            "GRANT SELECT ON t TO u DROP TABLE t",
            "(@P1 int)SELECT a FROM t WHERE b = @P1 DELETE FROM t",
            // Parser differentials: a number before the dot, unbalanced parentheses.
            "SELECT 1.COMMIT CREATE TABLE tempdb.dbo.x (a int)",
            "SELECT 1. EXEC sp_who",
            "SELECT 1e.EXEC sp_who",
            "SELECT 1) EXEC sp_who",
        ] {
            assert!(single_statement(text, Engine::Mssql).is_err(), "{text}");
            assert!(!reads_only(text, Engine::Mssql), "{text}");
            assert!(check(text, Engine::Mssql).problem.is_some(), "{text}");
        }
        // One statement each, with the words that legitimately appear inside one.
        for (text, read) in [
            ("SELECT a FROM t UNION ALL SELECT b FROM u", true),
            ("SELECT a FROM t EXCEPT SELECT b FROM u", true),
            ("WITH x AS (SELECT 1 AS a) SELECT a FROM x", true),
            ("SELECT a FROM t WITH (NOLOCK)", true),
            ("SELECT TOP 5 WITH TIES a FROM t ORDER BY a", true),
            ("SELECT a FROM t INNER MERGE JOIN u ON t.id = u.id", false),
            ("SELECT t.[exec], t.[begin] FROM t", true),
            ("SELECT x.exec FROM t AS x", true),
            ("SELECT a FROM t WHERE b = @begin", true),
            ("SELECT a FROM t ORDER BY a OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY", true),
            ("SELECT a FROM t OPTION (USE HINT ('FORCE_LEGACY_CARDINALITY_ESTIMATION'))", true),
            ("UPDATE t SET a = 1 WHERE b = 2", false),
            (
                "MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id)",
                false,
            ),
            ("INSERT INTO t (a) SELECT a FROM u", false),
            ("INSERT INTO t EXEC p", false),
            ("WITH x AS (SELECT 1 AS a) DELETE FROM t WHERE a IN (SELECT a FROM x)", false),
            ("ALTER TABLE t DROP COLUMN c", false),
            ("DROP TABLE IF EXISTS t", false),
            ("ALTER DATABASE d SET SINGLE_USER WITH ROLLBACK IMMEDIATE", false),
            ("GRANT SELECT ON t TO u WITH GRANT OPTION", false),
            ("CREATE PROCEDURE p AS BEGIN SELECT 1 END", false),
            ("CREATE OR ALTER PROCEDURE p AS BEGIN SET NOCOUNT ON SELECT 1 END", false),
            ("DENY EXECUTE ON SCHEMA::app TO u", false),
            ("CREATE NONCLUSTERED INDEX ix ON t (a) WITH (ONLINE = ON)", false),
            ("(@P1 nvarchar(4000))SELECT id FROM shop.customers WHERE email = @P1", false),
        ] {
            assert!(single_statement(text, Engine::Mssql).is_ok(), "{text}");
            assert_eq!(reads_only(text, Engine::Mssql), read, "{text}");
        }
    }

    #[test]
    fn every_server_setting_must_agree() {
        // Under MySQL's default mode `"a\" , …"` is one string; under ANSI_QUOTES `"a\"` is a name and SLEEP is a call.
        let text = "SELECT \"a\\\" , SLEEP(5) -- \"";
        assert!(!reads_only(text, Engine::Mysql), "{text}");
        // SQL Server with QUOTED_IDENTIFIER OFF reads `"…"` as a string: both readings still see one read.
        assert!(reads_only("SELECT \"x\" AS a FROM t", Engine::Mssql));
        assert!(reads_only("SELECT \"col\" FROM t", Engine::Mysql));
    }

    #[test]
    fn a_setting_where_the_text_never_parses_does_not_refuse_a_read() {
        // Valid in the server's default mode; under the other backslash setting the quote never closes, so a server
        // running that way rejects the whole text and nothing runs.
        assert!(reads_only(r"SELECT 'it\'s' AS s", Engine::Mysql));
        assert!(reads_only(r#"SELECT "a\"b" AS s"#, Engine::Mysql));
        assert!(reads_only(r"SELECT 'C:\' AS p", Engine::Postgres));
        // But MySQL runs multi-statement text one statement at a time: with backslashes literal, this is a SELECT, a
        // DELETE and an open quote, and the first two would run before the error.
        let text = r"SELECT 'a\'; DELETE FROM t; SELECT '";
        assert!(single_statement(text, Engine::Mysql).is_err(), "{text}");
        assert!(!reads_only(text, Engine::Mysql), "{text}");
        // Errors that don't depend on settings refuse the text in every reading.
        assert!(!reads_only("SELECT 1 /* open", Engine::Mysql));
        assert!(!reads_only("SELECT\u{a0}1", Engine::Postgres));
    }

    #[test]
    fn a_cursor_fetch_is_a_statement_but_offset_fetch_is_part_of_the_query() {
        assert!(!reads_only("SELECT 1 AS a\rFETCH NEXT FROM c", Engine::Mssql));
        assert!(!reads_only("SELECT name FROM sys.tables\u{c}FETCH NEXT FROM c", Engine::Mssql));
        let paged = "SELECT name FROM sys.tables ORDER BY name OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY";
        assert!(reads_only(paged, Engine::Mssql), "{paged}");
        assert!(reads_only("SELECT a FROM t ORDER BY a OFFSET 1 ROW FETCH FIRST 1 ROW ONLY", Engine::Mssql));
        assert!(reads_only("SELECT CASE WHEN a = 1 THEN 'x' END AS b FROM t", Engine::Mssql));
        assert!(!reads_only("SELECT 1 AS a END CONVERSATION @h", Engine::Mssql));
        assert!(!reads_only("SELECT 1 AS a BREAK", Engine::Mssql));
    }

    #[test]
    fn no_tsql_statement_keyword_hides_behind_any_separator() {
        let separators = [" ", "\n", "\t", "\r", "\r\n", "/**/", "/* note */", " -- note\n", " -- note\r", ".", ") "];
        for keyword in MSSQL_STATEMENTS.iter().filter(|k| **k != "WITH") {
            // The words that are statements only in context get it: END CONVERSATION, ADD SIGNATURE.
            let keyword = match *keyword {
                "END" => "END CONVERSATION",
                "ADD" => "ADD SIGNATURE",
                k => k,
            };
            for sep in separators {
                let text = format!("SELECT 1{sep}{keyword} x");
                assert!(!reads_only(&text, Engine::Mssql), "{text:?}");
                assert!(single_statement(&text, Engine::Mssql).is_err(), "{text:?}");
            }
        }
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment() {
        assert!(!reads_only("SELECT 1 -- note\r, pg_stat_statements_reset()", Engine::Postgres));
        assert!(single_statement("SELECT 1 -- note\rCOMMIT CREATE TABLE t (a int)", Engine::Mssql).is_err());
        assert!(!reads_only("SELECT 1 # note\r, GET_LOCK('a', 10)", Engine::Mysql));
        // An ordinary comment on its own line still reads.
        assert!(reads_only("SELECT 1 -- note\r\n", Engine::Postgres));
    }

    #[test]
    fn invisible_characters_never_hide_a_keyword() {
        for (text, engine) in [
            ("SELECT 1\u{00A0}EXEC sp_who", Engine::Mssql),
            ("SELECT 1\u{200B}EXEC sp_who", Engine::Mssql),
            ("SELECT 1\u{3000}EXEC sp_who", Engine::Mssql),
            ("SELECT 1\u{2028}COMMIT", Engine::Mssql),
            ("SELECT pg_stat_reset\u{FEFF}()", Engine::Postgres),
            ("SELECT 1\u{0000}; DROP TABLE t", Engine::Mysql),
            ("SELECT 1\u{0085}KILL 53", Engine::Mssql),
        ] {
            assert!(single_statement(text, engine).is_err(), "{text:?}");
            assert!(!reads_only(text, engine), "{text:?}");
        }
        // Non-ASCII inside strings, quoted names and plain identifiers is fine.
        assert!(reads_only("SELECT 'café\u{00A0}' AS x, [prénom] FROM t", Engine::Mssql));
        assert!(reads_only("SELECT prénom FROM clients", Engine::Postgres));
        // A real qualified name still reads.
        assert!(reads_only("SELECT t.exec, [t].begin FROM t", Engine::Mssql));
    }

    #[test]
    fn replication_lag_arithmetic_is_a_read() {
        assert!(reads_only(
            "SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn) FROM pg_stat_replication",
            Engine::Postgres
        ));
        assert!(!reads_only("SELECT pg_wal_replay_pause()", Engine::Postgres));
    }

    #[test]
    fn functions_with_side_effects_are_not_reads() {
        for text in [
            "SELECT pg_stat_statements_reset()",
            "SELECT pg_stat_reset_single_table_counters(0)",
            "SELECT pg_stat_reset()",
            "SELECT pg_create_logical_replication_slot('s', 'pgoutput')",
            "SELECT pg_drop_replication_slot('s')",
            "SELECT pg_log_backend_memory_contexts(1)",
            "SELECT * FROM crosstab('SELECT pg_terminate_backend(1)')",
            "SELECT pg_backup_start('x')",
            "SELECT \"pg_stat_statements_reset\"()",
        ] {
            assert!(!reads_only(text, Engine::Postgres), "{text}");
        }
        for text in ["SELECT NEXT VALUE FOR s", "SELECT nextval(s)", "SELECT setval(s, 5)", "SELECT SLEEP(10)"] {
            assert!(!reads_only(text, Engine::Mysql), "{text}");
        }
        // Ordinary functions still read.
        assert!(reads_only("SELECT lower(name), count(*) FROM t GROUP BY 1", Engine::Postgres));
        assert!(reads_only("SELECT pg_size_pretty(pg_total_relation_size('t'))", Engine::Postgres));
    }

    #[test]
    fn stacked_statements_are_refused() {
        for engine in ALL {
            assert!(single_statement("SELECT 1; DROP TABLE t", engine).is_err());
            assert!(single_statement("SELECT 1;\nSET x = 1;", engine).is_err());
            assert!(single_statement("SELECT 'unterminated", engine).is_err());
            assert!(single_statement("SELECT 1 /* open", engine).is_err());
        }
    }

    #[test]
    fn mysql_backslashes_and_comments_follow_the_server() {
        // Under NO_BACKSLASH_ESCAPES the quote closes early and `; DROP` becomes a second statement: refuse either way.
        assert!(single_statement(r"SELECT 'a\'; DROP TABLE t; -- '", Engine::Mysql).is_err());
        assert!(single_statement(r#"SELECT "a\"; DROP TABLE t; -- ""#, Engine::Mysql).is_err());
        // `--` without a following space is arithmetic in MySQL, so the `;` is real.
        assert!(single_statement("SELECT 1--1; DROP TABLE t", Engine::Mysql).is_err());
        assert_eq!(
            single_statement("SELECT 1 -- note; still a comment", Engine::Mysql).unwrap(),
            "SELECT 1 -- note; still a comment"
        );
        assert_eq!(single_statement("SELECT 1 # note; comment", Engine::Mysql).unwrap(), "SELECT 1 # note; comment");
        assert!(single_statement("SELECT 1 /*!80000 ; DROP TABLE t */", Engine::Mysql).is_err());
        assert!(single_statement("SELECT 1 /*M!100000 ; DROP TABLE t */", Engine::Mysql).is_err());
        assert!(single_statement("SELECT /*+ NO_INDEX(t) */ a FROM t", Engine::Mysql).is_ok());
        assert!(single_statement("SELECT `a;b` FROM t", Engine::Mysql).is_ok());
        // MySQL block comments don't nest: the first `*/` ends it.
        assert!(single_statement("SELECT 1 /* /* */ ; DROP TABLE t */", Engine::Mysql).is_err());
    }

    #[test]
    fn postgres_quoting_is_followed() {
        let pg = Engine::Postgres;
        assert!(single_statement("SELECT $$a;b$$, $tag$ ; $tag$", pg).is_ok());
        assert!(single_statement("SELECT * FROM t WHERE id = $1", pg).is_ok());
        assert!(single_statement("SELECT $$never closed; DROP TABLE t", pg).is_err());
        assert!(single_statement(r"SELECT E'a\'; b'", pg).is_ok());
        // With standard_conforming_strings off this closes early; with it on it doesn't: ambiguous, so refused.
        assert!(single_statement(r"SELECT 'a\'; DROP TABLE t; --'", pg).is_err());
        assert!(single_statement("SELECT 1 /* outer /* inner ; */ still */", pg).is_ok());
        assert!(single_statement(r#"SELECT "a;b" FROM t"#, pg).is_ok());
        assert!(single_statement("SELECT 1 # 2; DROP TABLE t", pg).is_err());
    }

    #[test]
    fn mssql_brackets_are_names() {
        assert!(single_statement("SELECT [a;b] FROM [t]]x]", Engine::Mssql).is_ok());
        assert!(single_statement("SELECT * FROM #temp; DROP TABLE t", Engine::Mssql).is_err());
    }

    #[test]
    fn checks_name_the_risk() {
        let pg = Engine::Postgres;
        assert!(check("select 1", pg).reads_only);
        let every_row = check("DELETE FROM orders", pg);
        assert_eq!((every_row.reads_only, every_row.verb.as_str()), (false, "DELETE"));
        assert!(every_row.warning.unwrap().contains("every row"));
        assert!(check("UPDATE orders SET note = '' WHERE id = 1", pg).warning.is_none());
        assert!(check("DROP TABLE t", Engine::Mssql).warning.unwrap().contains("removes"));
        assert!(check("SELECT * FROM t FOR UPDATE", pg).warning.unwrap().contains("can't prove"));
        assert!(check("SELECT 1; SELECT 2", pg).problem.is_some());
    }

    #[test]
    fn reads_are_told_from_writes() {
        for engine in ALL {
            assert!(reads_only("SELECT a, b FROM t WHERE c = 'DELETE'", engine));
            assert!(reads_only("  (SELECT 1)", engine));
            assert!(!reads_only("DELETE FROM t", engine));
            assert!(!reads_only("UPDATE t SET a = 1", engine));
            assert!(!reads_only("SELECT a INTO t2 FROM t", engine));
            assert!(!reads_only("SELECT 1; DELETE FROM t", engine));
            assert!(!reads_only("WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone", engine));
            assert!(!reads_only("SELECT * FROM t FOR UPDATE", engine));
            assert!(!reads_only("EXEC sp_who", engine));
            assert!(!reads_only("", engine));
        }
        assert!(reads_only("WITH x AS (SELECT 1) SELECT * FROM x", Engine::Mssql));
        assert!(!reads_only("SELECT * FROM t FOR SHARE", Engine::Postgres));
        assert!(!reads_only("SELECT * FROM t LOCK IN SHARE MODE", Engine::Mysql));
        assert!(!reads_only("SELECT NEXT VALUE FOR dbo.seq", Engine::Mssql));
        assert!(!reads_only("SELECT * FROM t WITH (UPDLOCK) WHERE id = 1", Engine::Mssql));
        assert!(!reads_only("SELECT * FROM OPENQUERY(remote, 'DELETE FROM t')", Engine::Mssql));
        assert!(!reads_only("SELECT pg_terminate_backend(123)", Engine::Postgres));
        assert!(!reads_only("SELECT set_config('default_transaction_read_only', 'off', false)", Engine::Postgres));
        assert!(!reads_only("SELECT pg_advisory_lock(1)", Engine::Postgres));
        assert!(!reads_only("SELECT query_to_xml('DELETE FROM t', true, true, '')", Engine::Postgres));
        assert!(!reads_only("EXPLAIN ANALYZE SELECT * FROM t", Engine::Postgres));
        assert!(reads_only("EXPLAIN SELECT * FROM t", Engine::Postgres));
        assert!(reads_only("SHOW work_mem", Engine::Postgres));
        assert!(!reads_only("SHOW work_mem", Engine::Mssql));
        assert!(!reads_only("SELECT GET_LOCK('x', 10)", Engine::Mysql));
        assert!(reads_only("SELECT nextval_count FROM stats", Engine::Postgres));
        assert!(reads_only("SELECT `update` FROM t", Engine::Mysql));
        assert!(!reads_only("SELECT 1 /*!80000 , (DELETE FROM t) */", Engine::Mysql));
        // Quoted names call functions too.
        assert!(!reads_only(r#"SELECT "pg_terminate_backend"(123)"#, Engine::Postgres));
        assert!(!reads_only(r#"SELECT pg_catalog."set_config"('a', 'b', false)"#, Engine::Postgres));
        assert!(!reads_only(r#"SELECT U&"pg_terminate_backend"(1)"#, Engine::Postgres));
        assert!(!reads_only("SELECT * FROM [openquery](remote, 'DELETE FROM t')", Engine::Mssql));
        assert!(!reads_only("SELECT `get_lock`('a', 1)", Engine::Mysql));
        assert!(!reads_only(r#"SELECT "get_lock"('a', 1)"#, Engine::Mysql), "an identifier under ANSI_QUOTES");
        assert!(reads_only(r#"SELECT "Name", [Order] FROM t"#, Engine::Mssql));
        assert!(reads_only(r#"SELECT "name" FROM "orders" WHERE "id" = 1"#, Engine::Postgres));
        assert!(!reads_only("SELECT * FROM t FOR KEY SHARE", Engine::Postgres));
    }

    fn mark(sql: &str) -> String {
        mark_mssql_statements(sql, "/* culprit */")
    }

    #[test]
    fn marks_every_statement_keyword() {
        assert_eq!(mark("SELECT a FROM t"), "SELECT /* culprit */ a FROM t");
        assert_eq!(
            mark("INSERT INTO t (a) SELECT a FROM s; DELETE FROM s;\nexec dbo.refresh"),
            "INSERT /* culprit */ INTO t (a) SELECT /* culprit */ a FROM s; DELETE /* culprit */ FROM s;\nexec /* culprit */ dbo.refresh"
        );
        assert_eq!(
            mark("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a;"),
            "MERGE /* culprit */ t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE /* culprit */ SET a = s.a;"
        );
        assert_eq!(mark("EXECUTE('SELECT 1')"), "EXECUTE /* culprit */('SELECT 1')");
        // A common table expression's statement is marked, and so is its own SELECT.
        assert_eq!(
            mark("WITH old AS (SELECT id FROM t WHERE done = 1) UPDATE old SET archived = 1"),
            "WITH old AS (SELECT /* culprit */ id FROM t WHERE done = 1) UPDATE /* culprit */ old SET archived = 1"
        );
        // A batch that declares first: the SELECT after it is the statement Query Store keeps.
        assert_eq!(
            mark(
                "DECLARE @since datetime2 = DATEADD(day, -1, SYSUTCDATETIME());\nSELECT COUNT(*) FROM t WHERE at > @since"
            ),
            "DECLARE @since datetime2 = DATEADD(day, -1, SYSUTCDATETIME());\nSELECT /* culprit */ COUNT(*) FROM t WHERE at > @since"
        );
    }

    #[test]
    fn never_marks_inside_strings_names_or_comments() {
        // The value written stays the person's.
        assert_eq!(mark("UPDATE t SET note = 'select'"), "UPDATE /* culprit */ t SET note = 'select'");
        assert_eq!(
            mark("UPDATE t SET note = N'it''s a delete'"),
            "UPDATE /* culprit */ t SET note = N'it''s a delete'"
        );
        assert_eq!(mark("-- delete me\nSELECT 1"), "-- delete me\nSELECT /* culprit */ 1");
        assert_eq!(
            mark("/* select /* nested update */ exec */ SELECT 1"),
            "/* select /* nested update */ exec */ SELECT /* culprit */ 1"
        );
        assert_eq!(
            mark("SELECT [select], [a]]update] FROM [delete]"),
            "SELECT /* culprit */ [select], [a]]update] FROM [delete]"
        );
        assert_eq!(mark(r#"SELECT "update" FROM t"#), r#"SELECT /* culprit */ "update" FROM t"#);
        assert_eq!(
            mark("SELECT @select, selected FROM #select"),
            "SELECT /* culprit */ @select, selected FROM #select"
        );
    }

    #[test]
    fn leaves_text_without_a_statement_keyword_or_with_a_module_body_as_it_is() {
        for sql in [
            "DECLARE @x int = 1",
            "SET NOCOUNT ON",
            "TRUNCATE TABLE t",
            "CREATE PROCEDURE dbo.p AS SELECT 1",
            "create or alter view dbo.v as select a from t",
            "ALTER FUNCTION dbo.f() RETURNS TABLE AS RETURN (SELECT 1 AS a)",
            "CREATE TRIGGER tr ON t AFTER INSERT AS UPDATE t SET a = 1",
            // Text the lexer can't read is sent as written.
            "SELECT 'never closed",
        ] {
            assert_eq!(mark(sql), sql);
        }
    }

    #[test]
    fn a_parameterized_statement_starts_after_its_parameter_list() {
        assert_eq!(
            without_parameter_list("(@par0 nvarchar(4000))update [app].[conversations] set n = n + 1"),
            "update [app].[conversations] set n = n + 1"
        );
        assert_eq!(without_parameter_list("  (@P1 int,@P2 decimal(10, 2))SELECT 1"), "SELECT 1");
        assert_eq!(without_parameter_list("(SELECT 1) UNION (SELECT 2)"), "(SELECT 1) UNION (SELECT 2)");
        assert_eq!(without_parameter_list("(@P1 int"), "(@P1 int");
    }
}

#[cfg(test)]
mod display_evidence_tests {
    use super::*;

    #[test]
    fn compound_updates_are_never_isolated_allocators() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
            assert!(!isolated_counter_update(
                "UPDATE orders SET n = n + 1, changed_at = CURRENT_TIMESTAMP RETURNING n",
                engine,
                "n"
            ));
            assert!(!isolated_counter_update("UPDATE orders SET n = n + 1 RETURNING n, status", engine, "n"));
            assert!(!isolated_counter_update("UPDATE orders SET status = 1 RETURNING n", engine, "n"));
        }
        assert!(isolated_counter_update(
            "UPDATE orders SET n = n + 1 OUTPUT inserted.n WHERE id = @id",
            Engine::Mssql,
            "n"
        ));
        assert!(isolated_counter_update("UPDATE orders SET n = n + 1 RETURNING n", Engine::Postgres, "n"));
        assert!(isolated_counter_update(
            "UPDATE orders SET n = LAST_INSERT_ID(n + 1) WHERE id = ?",
            Engine::Mysql,
            "n"
        ));
        assert!(isolated_counter_update("UPDATE orders SET n = n + 1 RETURNING n", Engine::Sqlite, "n"));
    }

    #[test]
    fn cte_targets_resolve_only_unambiguous_sources_on_every_engine() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
            assert_eq!(cte_write_target("/* task */ WITH pending AS (SELECT * FROM shop.orders WHERE state = 0) UPDATE pending SET state = 1", engine).as_deref(), Some("shop.orders"));
            assert_eq!(
                cte_write_target("WITH a AS (SELECT * FROM orders), b AS (SELECT * FROM a) DELETE FROM b", engine)
                    .as_deref(),
                Some("orders")
            );
            for sql in [
                "WITH a AS (SELECT * FROM orders o JOIN products p ON p.id = o.id) UPDATE a SET state = 1",
                "WITH a AS (SELECT * FROM orders o, products p) UPDATE a SET state = 1",
                "WITH a AS (SELECT * FROM a) UPDATE a SET state = 1",
                "WITH a AS (SELECT * FROM orders UNION SELECT * FROM products) UPDATE a SET state = 1",
            ] {
                assert_eq!(cte_write_target(sql, engine), None, "{engine:?}: {sql}");
            }
        }
        assert_eq!(cte_write_target("WITH [pending] AS (SELECT TOP (10) * FROM [shop].[orders] WITH (UPDLOCK, READPAST)) UPDATE [pending] SET state = 1", Engine::Mssql).as_deref(), Some("shop.orders"));
    }
}
