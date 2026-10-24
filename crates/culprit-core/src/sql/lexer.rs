//! Low-level SQL lexical scanning across engine quoting and comment modes, shared by statement safety,
//! static predicate matching, and plan predicate inspection.

use crate::{Engine, SqlSyntax};

/// How backslashes behave inside quoted text. MySQL and Postgres both have a server setting that flips this
/// (NO_BACKSLASH_ESCAPES, standard_conforming_strings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backslash {
    Escapes,
    Literal,
}

/// What `"…"` is. MySQL's ANSI_QUOTES and SQL Server's QUOTED_IDENTIFIER flip it between a string and a name,
/// independently of the backslash setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DoubleQuote {
    String,
    Identifier,
}

/// One combination of the server settings that change how text is lexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mode {
    pub(crate) backslash: Backslash,
    pub(crate) double_quote: DoubleQuote,
}

/// Every combination a server can be running with: the text is lexed under each and every reading must agree, so a
/// session setting Culprit didn't choose can never make the server see a statement the lexer didn't.
pub(crate) fn modes(engine: Engine) -> &'static [Mode] {
    use Backslash::*;
    use DoubleQuote::*;
    match engine.sql_syntax() {
        // QUOTED_IDENTIFIER ON (the driver default) and OFF.
        SqlSyntax::Mssql => {
            &[Mode { backslash: Literal, double_quote: Identifier }, Mode { backslash: Literal, double_quote: String }]
        }
        // standard_conforming_strings on and off (E'…' always escapes).
        SqlSyntax::Postgres => &[
            Mode { backslash: Escapes, double_quote: Identifier },
            Mode { backslash: Literal, double_quote: Identifier },
        ],
        // No backslash escapes. A double-quoted word is a name, or a string when no such name exists (SQLite's legacy rule).
        SqlSyntax::Sqlite => {
            &[Mode { backslash: Literal, double_quote: Identifier }, Mode { backslash: Literal, double_quote: String }]
        }
        // NO_BACKSLASH_ESCAPES × ANSI_QUOTES.
        SqlSyntax::Mysql => &[
            Mode { backslash: Escapes, double_quote: String },
            Mode { backslash: Literal, double_quote: String },
            Mode { backslash: Escapes, double_quote: Identifier },
            Mode { backslash: Literal, double_quote: Identifier },
        ],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    Word(&'a str),
    /// A quoted identifier's raw text between its delimiters (`"name"`, `[name]`, `` `name` ``). The server treats it
    /// like a word (it can name a function), so the read check must see it.
    Quoted(&'a str, u8),
    Semicolon(usize),
    Punct(char),
}

/// Top-level tokens, with literals and comments skipped. Errors on text it can't lex safely.
pub(crate) fn lex(text: &str, engine: Engine, mode: Mode) -> std::result::Result<Vec<Token<'_>>, &'static str> {
    let mut tokens = Vec::new();
    lex_into(text, engine, mode, &mut tokens).map(|()| tokens)
}

/// The message for a quote that never closes: the one lexing error that depends on the server's settings.
pub(crate) const UNCLOSED_QUOTE: &str = "A quoted string or name is never closed";

/// Every reading of `text` under the server settings that change lexing (see [`modes`]). A setting under which the
/// text has a quote that never closes is one where the server rejects the text whole, so that reading is left out — as
/// long as nothing before the open quote could run on its own: MySQL runs multi-statement text a statement at a time, so
/// a `;` before it keeps the error. Every other error (comments, executable comments, invisible characters) refuses the
/// text in any reading, and at least one reading must succeed.
pub(crate) fn readings(text: &str, engine: Engine) -> std::result::Result<Vec<Vec<Token<'_>>>, &'static str> {
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
    let engine = engine.sql_syntax();
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
            b'-' if next == Some(b'-')
                && (engine != SqlSyntax::Mysql || bytes.get(i + 2).is_none_or(|b| *b <= b' ')) =>
            {
                i = line_end(bytes, i);
            }
            b'#' if engine == SqlSyntax::Mysql => i = line_end(bytes, i),
            b'/' if next == Some(b'*') => {
                if engine == SqlSyntax::Mysql && matches!(bytes.get(i + 2), Some(b'!') | Some(b'M')) {
                    // `/*! … */` and MariaDB's `/*M! … */` are executed by the server, so they are code, not comments.
                    let executable = bytes.get(i + 2) == Some(&b'!') || bytes.get(i + 3) == Some(&b'!');
                    if executable {
                        return Err("Executable comments (/*! … */) aren't allowed here");
                    }
                }
                i = block_comment_end(bytes, i, engine != SqlSyntax::Mysql)?;
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
            b'`' if matches!(engine, SqlSyntax::Mysql | SqlSyntax::Sqlite) => {
                let end = quoted_end(bytes, i, b'`', false)?;
                tokens.push(Token::Quoted(&text[i + 1..end - 1], b'`'));
                i = end;
            }
            b'[' if matches!(engine, SqlSyntax::Mssql | SqlSyntax::Sqlite) => {
                let end = quoted_end(bytes, i, b']', false)?;
                tokens.push(Token::Quoted(&text[i + 1..end - 1], b']'));
                i = end;
            }
            b'$' if engine == SqlSyntax::Postgres => match dollar_tag(bytes, i) {
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
                if engine == SqlSyntax::Postgres && word.eq_ignore_ascii_case("e") && bytes.get(i) == Some(&b'\'') {
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
pub(crate) fn invisible(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || matches!(
            ch,
            '\u{00AD}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{FEFF}'
        )
}

pub(crate) fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c >= 0x80
}

pub(crate) fn is_word_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'@' || c == b'#' || c >= 0x80
}

/// Where a `--` or `#` comment ends. Servers end it at a carriage return as well as a line feed (Postgres does, and
/// so may others): ending it at either one never hides text the server runs, so `-- note\rEXEC …` shows its EXEC.
pub(crate) fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..].iter().position(|b| matches!(b, b'\n' | b'\r')).map_or(bytes.len(), |p| from + p + 1)
}

/// The index after a `/* … */` comment starting at `from`. Postgres and SQL Server nest these; MySQL doesn't.
pub(crate) fn block_comment_end(bytes: &[u8], from: usize, nests: bool) -> std::result::Result<usize, &'static str> {
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
pub(crate) fn quoted_end(
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

/// Best-effort index after a quoted run opened at `from` with `close`, advancing to the end of `bytes` when unclosed.
pub(crate) fn skip_quoted_bytes(bytes: &[u8], from: usize, close: u8, backslash_escapes: bool) -> usize {
    quoted_end(bytes, from, close, backslash_escapes).unwrap_or(bytes.len())
}

/// A Postgres dollar-quote opener at `from` (`$$` or `$tag$`); `$1` is a parameter, not a quote.
pub(crate) fn dollar_tag(bytes: &[u8], from: usize) -> Option<&[u8]> {
    let rest = &bytes[from + 1..];
    let len = rest.iter().position(|b| *b == b'$')?;
    let tag = &rest[..len];
    let valid = tag.first().is_none_or(|b| b.is_ascii_alphabetic() || *b == b'_' || *b >= 0x80)
        && tag.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80);
    valid.then_some(&bytes[from..from + len + 2])
}

pub(crate) fn dollar_quoted_end(bytes: &[u8], from: usize, tag: &[u8]) -> std::result::Result<usize, &'static str> {
    let body = from + tag.len();
    bytes[body..]
        .windows(tag.len())
        .position(|w| w == tag)
        .map(|p| body + p + tag.len())
        .ok_or("A $$-quoted string is never closed")
}

/// One name part as written in a plan or filter, without its delimiters:
/// `[a]]b]` → `a]b`, `` `a``b` `` → ``a`b``, `"a""b"` → `a"b`.
pub(crate) fn clean_identifier(raw: &str) -> String {
    let raw = raw.trim();
    let inner = |open: char, close: char| {
        raw.strip_prefix(open)
            .and_then(|r| r.strip_suffix(close))
            .map(|r| r.replace(&format!("{close}{close}"), &close.to_string()))
    };
    inner('[', ']')
        .or_else(|| inner('`', '`'))
        .or_else(|| inner('"', '"'))
        .unwrap_or_else(|| raw.trim_matches(|c| c == '[' || c == ']' || c == '`' || c == '"').to_string())
}

/// Split a dotted identifier (`[db].[s].[t]`, `"s"."t"`, `s.t`) into unescaped parts while respecting quoted dots.
pub(crate) fn identifier_parts(value: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quoted = None;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        part.push(c);
        if let Some(close) = quoted {
            if c == close {
                if chars.peek() == Some(&close) {
                    part.push(chars.next()?);
                } else {
                    quoted = None;
                }
            }
        } else {
            match c {
                '[' => quoted = Some(']'),
                '"' => quoted = Some('"'),
                '`' => quoted = Some('`'),
                '.' => {
                    part.pop();
                    parts.push(clean_identifier(&part));
                    part.clear();
                }
                _ => {}
            }
        }
    }
    if quoted.is_some() {
        return None;
    }
    parts.push(clean_identifier(&part));
    (!parts.iter().any(String::is_empty)).then_some(parts)
}

/// The text up to the parenthesis that closes the one just opened, skipping over quoted strings and delimited names.
pub(crate) fn balanced_parens(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut depth = 1i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                i = skip_quoted_bytes(bytes, i, bytes[i], false);
                continue;
            }
            b'[' => {
                i = skip_quoted_bytes(bytes, i, b']', false);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[..i];
                }
            }
            _ => {}
        }
        i += 1;
    }
    text
}

/// Strip outer balanced parentheses and surrounding whitespace: `((a = 1))` → `a = 1`.
pub(crate) fn strip_outer_parens(mut text: &str) -> &str {
    loop {
        let s = text.trim();
        if s.starts_with('(') && s.ends_with(')') && balanced_parens(&s[1..]).len() == s.len() - 2 {
            text = &s[1..s.len() - 1];
        } else {
            return s;
        }
    }
}

/// The parts of a condition ANDed at its top level, outer parentheses removed:
/// `((a = 1) AND (b > 2 OR c < 3))` → `a = 1`, `b > 2 OR c < 3`.
/// Respects quoted strings (`'a and b'`), delimited identifiers (`[a and b]`) and `BETWEEN x AND y`.
pub(crate) fn split_top_level_and(text: &str) -> Vec<&str> {
    let text = strip_outer_parens(text);
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut between_pending = false;
    let mut i = 0;
    let boundary_before = |pos: usize| pos > 0 && (bytes[pos - 1].is_ascii_whitespace() || bytes[pos - 1] == b')');
    let boundary_after = |pos: usize| {
        pos < bytes.len()
            && (bytes[pos].is_ascii_whitespace() || matches!(bytes[pos], b'(' | b'[' | b'"' | b'`' | b'\''))
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                i = skip_quoted_bytes(bytes, i, bytes[i], false);
                continue;
            }
            b'[' => {
                i = skip_quoted_bytes(bytes, i, b']', false);
                continue;
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0
                && boundary_before(i)
                && bytes.get(i..i + 7).is_some_and(|w| w.eq_ignore_ascii_case(b"between"))
                && boundary_after(i + 7) =>
            {
                between_pending = true;
                i += 7;
                continue;
            }
            _ if depth == 0
                && boundary_before(i)
                && bytes.get(i..i + 3).is_some_and(|w| w.eq_ignore_ascii_case(b"and"))
                && boundary_after(i + 3) =>
            {
                if between_pending {
                    between_pending = false;
                    i += 3;
                    continue;
                }
                parts.push(strip_outer_parens(&text[start..i]));
                start = i + 3;
                i += 3;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(strip_outer_parens(&text[start..]));
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}
