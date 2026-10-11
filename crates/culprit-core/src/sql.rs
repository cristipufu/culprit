//! Statement-level SQL lexing for every engine: enough to tell one statement from several and reads from writes, without
//! a parser. Text from a workload or an agent is untrusted, so the rules are conservative: anything this can't lex with
//! confidence is treated as unsafe. It is a guard in front of the engines' own protections (read-only sessions and
//! transactions, prepared statements), never a replacement for them.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{Engine, Error, Result};

/// How backslashes behave inside quoted text. MySQL and Postgres both have a server setting that flips this
/// (NO_BACKSLASH_ESCAPES, standard_conforming_strings), so their text is lexed both ways and must agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backslash {
    Escapes,
    Literal,
}

fn modes(engine: Engine) -> &'static [Backslash] {
    match engine {
        Engine::Mssql => &[Backslash::Literal],
        Engine::Postgres | Engine::Mysql => &[Backslash::Escapes, Backslash::Literal],
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
fn lex(text: &str, engine: Engine, backslash: Backslash) -> std::result::Result<Vec<Token<'_>>, &'static str> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
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
                // A string in MySQL's default mode (backslashes escape), an identifier elsewhere and under ANSI_QUOTES.
                let string = engine == Engine::Mysql && backslash == Backslash::Escapes;
                let end = quoted_end(bytes, i, b'"', string)?;
                if !string {
                    tokens.push(Token::Quoted(&text[i + 1..end - 1], b'"'));
                }
                i = end;
            }
            b'`' if engine == Engine::Mysql => {
                let end = quoted_end(bytes, i, b'`', false)?;
                tokens.push(Token::Quoted(&text[i + 1..end - 1], b'`'));
                i = end;
            }
            b'[' if engine == Engine::Mssql => {
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
            _ if is_word_start(c) => {
                let start = i;
                while i < bytes.len() && is_word_part(bytes[i]) {
                    i += 1;
                }
                let word = &text[start..i];
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
    Ok(tokens)
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c >= 0x80
}

fn is_word_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'@' || c == b'#' || c >= 0x80
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |p| from + p + 1)
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
    Err("A quoted string or name is never closed")
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
    for (n, &mode) in modes(engine).iter().enumerate() {
        let tokens = lex(text, engine, mode).map_err(Error::invalid)?;
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
    if statement.is_empty() || lex(statement, engine, modes(engine)[0]).map_err(Error::invalid)?.is_empty() {
        return Err(Error::invalid("There is no statement to run"));
    }
    Ok(statement)
}

/// Statements a read starts with, per engine.
fn read_starts(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Mssql => &["SELECT", "WITH"],
        Engine::Postgres => &["SELECT", "WITH", "TABLE", "VALUES", "SHOW", "EXPLAIN"],
        Engine::Mysql => &["SELECT", "WITH", "TABLE", "VALUES", "SHOW", "EXPLAIN", "DESCRIBE", "DESC"],
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
    }
}

/// True when `text` is one statement that only reads: it starts like a read, contains no data-changing clause
/// (data-modifying CTE, SELECT INTO, locking reads, EXPLAIN ANALYZE) and calls no built-in with side effects.
/// User-defined functions can still have side effects, so callers also run reads in a read-only transaction where the
/// engine has one, and a read-only login is the only complete guarantee.
pub fn reads_only(text: &str, engine: Engine) -> bool {
    let Ok(statement) = single_statement(text, engine) else { return false };
    let advisory = |w: &str| engine == Engine::Postgres && w.to_ascii_lowercase().contains("advisory");
    modes(engine).iter().all(|&mode| {
        let Ok(tokens) = lex(statement, engine, mode) else { return false };
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
        let unsafe_call = |name: &str| functions.iter().any(|f| f.eq_ignore_ascii_case(name)) || advisory(name);
        let keywords_ok = words.iter().enumerate().all(|(n, (at, word))| {
            let previous = n.checked_sub(1).map(|p| words[p].1.as_str());
            let calls = matches!(tokens.get(at + 1), Some(Token::Punct('(')));
            let locking = word == "SHARE" && matches!(previous, Some("FOR") | Some("IN") | Some("KEY"));
            let sequence = engine == Engine::Mssql && word == "VALUE" && previous == Some("NEXT");
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
    let words: Vec<String> = lex(statement, engine, modes(engine)[0])
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
