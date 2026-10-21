//! SQL literal, prose, and four-part identifier redaction across all engine dialects.

use crate::model::Engine;

/// Query hints whose number tunes the plan (`OPTION (MAXDOP 1)`, `FAST 10`), not data: what a plan's reading of the
/// statement depends on, kept.
const HINTS_WITH_A_NUMBER: &[&str] = &["MAXDOP", "FAST", "MAXRECURSION", "QUERYTRACEON"];

/// Numbers inside these types' parentheses are sizes (`nvarchar(200)`, `decimal(12,2)`), not data.
const SIZED_TYPES: &[&str] = &[
    "char",
    "varchar",
    "nchar",
    "nvarchar",
    "binary",
    "varbinary",
    "decimal",
    "numeric",
    "float",
    "datetime2",
    "datetimeoffset",
    "time",
    "timestamp",
    "timestamptz",
    "varying",
    "bit",
    "interval",
];

/// Replace literal values in SQL text (a statement or a plan predicate) with placeholders.
pub fn redact_sql(text: &str, engine: Engine) -> String {
    redact(text, engine, true)
}

/// Prose that quotes SQL in `code spans` (a finding's summary, caveats, notes): the values inside the spans become
/// placeholders; the words around them stay as written, apostrophes included. Checks put every piece of SQL they quote
/// in a code span, the parameter values a plan was compiled for too.
pub fn redact_prose(text: &str, engine: Engine) -> String {
    // Values first, wherever they are: a value can hold a backtick, which would end its code span early and leave the
    // values after it looking like prose.
    mask_prose_values(text, engine)
        .split('`')
        .enumerate()
        .map(|(i, part)| if i % 2 == 1 { collapse_parameters(redact_sql(part, engine)) } else { part.to_string() })
        .collect::<Vec<_>>()
        .join("`")
}

/// String literals and GUIDs anywhere in prose become placeholders. A quote right after a letter or digit is an
/// apostrophe (isn't, the plan's), unless that letter stands alone as a string prefix (`N'…'`, `E'…'`, `X'…'`,
/// `B'…'`). A literal that never closes hides the rest of the text: better a sentence lost than a value leaked.
fn mask_prose_values(text: &str, engine: Engine) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let prev = i.checked_sub(1).and_then(|k| chars.get(k));
        if chars[i] == '\'' {
            let prefix = prev.is_some_and(|p| matches!(p.to_ascii_uppercase(), 'N' | 'E' | 'X' | 'B'))
                && !word(i.checked_sub(2).and_then(|k| chars.get(k)));
            if !word(prev) || prefix {
                let backslashes = engine == Engine::Mysql || prev.is_some_and(|p| p.eq_ignore_ascii_case(&'E'));
                let mut j = i + 1;
                let close = loop {
                    match chars.get(j) {
                        None => break None,
                        Some('\\') if backslashes => j += 2,
                        Some('\'') if chars.get(j + 1) == Some(&'\'') => j += 2,
                        Some('\'') => break Some(j),
                        Some(_) => j += 1,
                    }
                };
                out.push_str("'?'");
                match close {
                    Some(end) => i = end + 1,
                    None => return out,
                }
                continue;
            }
        }
        if !word(prev) && is_guid(&chars[i..]) {
            out.push('?');
            i += 36;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `chars` starts with a GUID (8-4-4-4-12 hex digits) that isn't part of a longer word.
fn is_guid(chars: &[char]) -> bool {
    chars.len() >= 36
        && chars[..36].iter().enumerate().all(|(n, c)| match n {
            8 | 13 | 18 | 23 => *c == '-',
            _ => c.is_ascii_hexdigit(),
        })
        && !chars.get(36).is_some_and(|c| c.is_alphanumeric() || *c == '_')
}

/// A list of parameters whose values are all placeholders now (`@p0 = N'?', @p1 = N'?', …`) says nothing past its
/// first item.
fn collapse_parameters(span: String) -> String {
    let items: Vec<&str> = span.split(", ").collect();
    let placeholder = |item: &str| {
        item.split_once(" = ").is_some_and(|(name, value)| {
            name.starts_with(['@', '$', ':']) && matches!(value, "?" | "(?)" | "'?'" | "N'?'" | "E'?'")
        })
    };
    if items.len() > 1 && items.iter().all(|i| placeholder(i)) { format!("{}, …", items[0]) } else { span }
}

/// Strings only: for text where bare numbers are measurements (costs, row counts), not data.
pub fn redact_strings(text: &str, engine: Engine) -> String {
    redact(text, engine, false)
}

fn redact(text: &str, engine: Engine, numbers: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    // For each open parenthesis: whether it belongs to a sized type, so numbers in it are sizes.
    let mut parens: Vec<bool> = Vec::new();
    let mut last_word = String::new();
    let mut i = 0;
    let at = |k: usize| chars.get(k).copied();
    while i < chars.len() {
        let c = chars[i];
        let next = at(i + 1);
        match c {
            '-' if next == Some('-') && (engine != Engine::Mysql || at(i + 2).is_none_or(|n| n.is_whitespace())) => {
                let end = chars[i..].iter().position(|&x| x == '\n').map_or(chars.len(), |p| i + p);
                out.push_str(if end > i + 2 { "-- …" } else { "--" });
                i = end;
            }
            '#' if engine == Engine::Mysql => {
                let end = chars[i..].iter().position(|&x| x == '\n').map_or(chars.len(), |p| i + p);
                out.push_str("# …");
                i = end;
            }
            '/' if next == Some('*') => {
                let end = find(&chars, i + 2, &['*', '/']).map_or(chars.len(), |p| p + 2);
                let body: String = chars[i + 2..end.saturating_sub(2).max(i + 2)].iter().collect();
                // Optimizer hints and Culprit's own tag carry no data; any other comment might.
                if body.trim_start().starts_with('+') || body.trim() == "culprit" {
                    out.extend(&chars[i..end]);
                } else {
                    out.push_str("/* … */");
                }
                i = end;
            }
            '\'' => {
                let escapes = engine == Engine::Mysql || last_word.eq_ignore_ascii_case("e");
                let end = skip_quoted(&chars, i, '\'', escapes);
                // A pattern keeps where its wildcards are (`'%?%'`): that's its shape, not data, and a leading % is
                // what stops an index from being used.
                let body = &chars[(i + 1).min(end)..end.saturating_sub(1).max(i + 1).min(chars.len())];
                out.push('\'');
                if body.first() == Some(&'%') {
                    out.push('%');
                }
                out.push('?');
                if body.len() > 1 && body.last() == Some(&'%') {
                    out.push('%');
                }
                out.push('\'');
                i = end;
                last_word.clear();
            }
            '"' if engine == Engine::Mysql => {
                i = skip_quoted(&chars, i, '"', true);
                out.push_str("\"?\"");
                last_word.clear();
            }
            '"' => {
                let end = skip_quoted(&chars, i, '"', false);
                out.extend(&chars[i..end]);
                last_word = chars[i + 1..end.saturating_sub(1).max(i + 1)].iter().collect();
                i = end;
            }
            '[' if engine == Engine::Mssql => {
                let end = skip_quoted(&chars, i, ']', false);
                out.extend(&chars[i..end]);
                last_word = chars[i + 1..end.saturating_sub(1).max(i + 1)].iter().collect();
                i = end;
            }
            '`' if engine == Engine::Mysql => {
                let end = skip_quoted(&chars, i, '`', false);
                out.extend(&chars[i..end]);
                last_word.clear();
                i = end;
            }
            '$' if engine == Engine::Postgres => {
                if let Some(tag_end) = dollar_tag(&chars, i) {
                    let tag: Vec<char> = chars[i..=tag_end].to_vec();
                    let close = find(&chars, tag_end + 1, &tag).map_or(chars.len(), |p| p + tag.len());
                    out.push_str("'?'");
                    i = close;
                } else {
                    // `$1`: a parameter marker, kept.
                    let end = word_end(&chars, i + 1);
                    out.extend(&chars[i..end]);
                    i = end;
                }
                last_word.clear();
            }
            '@' | ':' if next.is_some_and(|n| n.is_alphanumeric() || n == '_' || n == '@') => {
                // `@P1`, `@@ROWCOUNT`, `:name`: markers and variables, not data. (`::` casts fall through below.)
                if c == ':' && next == Some(':') {
                    out.push_str("::");
                    i += 2;
                    continue;
                }
                let end = word_end(&chars, i + 1 + usize::from(next == Some('@')));
                out.extend(&chars[i..end]);
                last_word.clear();
                i = end;
            }
            '(' => {
                parens.push(SIZED_TYPES.iter().any(|t| t.eq_ignore_ascii_case(&last_word)));
                out.push(c);
                last_word.clear();
                i += 1;
            }
            ')' => {
                parens.pop();
                out.push(c);
                i += 1;
            }
            _ if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) => {
                let end = number_end(&chars, i);
                let hint = HINTS_WITH_A_NUMBER.iter().any(|h| h.eq_ignore_ascii_case(&last_word));
                if numbers && !parens.last().copied().unwrap_or(false) && !hint {
                    out.push('?');
                } else {
                    out.extend(&chars[i..end]);
                }
                i = end;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let end = word_end(&chars, i);
                last_word = chars[i..end].iter().collect();
                out.push_str(&last_word);
                i = end;
            }
            _ => {
                if !c.is_whitespace() && c != ',' {
                    last_word.clear();
                }
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// SQL Server names columns in plans as `[database].[schema].[table].[column]`. Drop the database part, so an export
/// doesn't name the database and plans from two environments read the same.
pub fn strip_database(text: &str) -> String {
    strip_parts(text, 3)
}

pub(super) fn strip_parts(text: &str, keep: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '[' || (i > 0 && (chars[i - 1] == '.' || chars[i - 1] == ']')) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Up to four bracketed parts joined by dots.
        let mut parts = Vec::new();
        let mut j = i;
        while j < chars.len() && chars[j] == '[' && parts.len() < 4 {
            let end = skip_quoted(&chars, j, ']', false);
            parts.push((j, end));
            if chars.get(end) == Some(&'.') && chars.get(end + 1) == Some(&'[') {
                j = end + 1;
            } else {
                break;
            }
        }
        let from = if parts.len() > keep { parts[parts.len() - keep].0 } else { i };
        let to = parts.last().map_or(i + 1, |p| p.1);
        out.extend(&chars[from..to]);
        i = to;
    }
    out
}

fn find(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || from > chars.len() {
        return None;
    }
    chars[from..].windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

/// The index after a quoted run opened at `start`; a doubled closing quote is an escaped one.
fn skip_quoted(chars: &[char], start: usize, close: char, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        if backslash && chars[i] == '\\' {
            i += 2;
        } else if chars[i] == close {
            if chars.get(i + 1) == Some(&close) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    chars.len()
}

fn word_end(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$' | '#' | '@')) {
        i += 1;
    }
    i
}

/// Digits, a decimal point, an exponent, hex (`0x1F`).
fn number_end(chars: &[char], from: usize) -> usize {
    let mut i = from;
    if chars.get(i) == Some(&'0') && matches!(chars.get(i + 1), Some('x' | 'X')) {
        i += 2;
        while i < chars.len() && chars[i].is_ascii_hexdigit() {
            i += 1;
        }
        return i;
    }
    while i < chars.len() {
        let c = chars[i];
        let exponent_sign = matches!(c, '+' | '-') && matches!(chars.get(i.wrapping_sub(1)), Some('e' | 'E'));
        if c.is_ascii_digit() || c == '.' || matches!(c, 'e' | 'E') || exponent_sign {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// A Postgres dollar-quote opener at `start` (`$$` or `$tag$`): the index of its closing `$`. `$1` is not one.
fn dollar_tag(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while i < chars.len() {
        match chars[i] {
            '$' => return Some(i),
            c if c.is_alphanumeric() || c == '_' => i += 1,
            _ => return None,
        }
    }
    None
}
