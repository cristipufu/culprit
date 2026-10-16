//! The log formats Culprit reads, one module each. A new framework is a module implementing `Format` and a line in
//! [`formats`].

mod django;
mod ecto;
mod efcore;
mod jsonl;
mod rails;

use crate::{Format, Moment};

/// Every format, in the order they're tried when two recognize the same number of lines.
pub fn formats() -> Vec<Box<dyn Format>> {
    vec![
        Box::new(rails::Rails::default()),
        Box::new(django::Django),
        Box::new(efcore::EfCore::default()),
        Box::new(ecto::Ecto::default()),
        Box::new(jsonl::JsonLines),
    ]
}

/// A line without terminal colour codes (`\e[1m\e[36m…\e[0m`), which development logs are full of.
pub(crate) fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A timestamp at the start of a line (`2026-10-15 12:00:05,120 …`, `2026-10-15T12:00:05.120Z …`) and the rest of the
/// line. Without a zone it is local time, whose zone the log doesn't say.
pub(crate) fn leading_timestamp(line: &str) -> Option<(Moment, &str)> {
    let t = line.trim_start();
    let bytes = t.as_bytes();
    let date_like = bytes.len() >= 19
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b' ' || bytes[10] == b'T')
        && bytes[13] == b':';
    if !date_like {
        return None;
    }
    // The timestamp runs to the first space after the time (and a zone right after it, as `+0000` or `Z`).
    let after_time = t[11..].find(' ').map_or(t.len(), |p| p + 11);
    let mut stamp = t[..after_time].replace(',', ".");
    let mut rest = &t[after_time..];
    let zoned_inline = stamp.ends_with('Z') || stamp[19..].contains(['+', '-']);
    let mut zoned = zoned_inline;
    if !zoned_inline {
        let next = rest.trim_start();
        if let Some(zone) = next.split_whitespace().next().filter(|z| is_zone(z)) {
            stamp.push(' ');
            stamp.push_str(zone);
            rest = &next[zone.len()..];
            zoned = true;
        }
    }
    let epoch = culprit_core::format::epoch_seconds(&stamp)?;
    Some((Moment { epoch, zoned }, rest))
}

fn is_zone(word: &str) -> bool {
    word == "UTC"
        || word == "Z"
        || ((word.starts_with('+') || word.starts_with('-'))
            && word.len() >= 5
            && word[1..].chars().all(|c| c.is_ascii_digit() || c == ':'))
}

/// A duration with its unit (`12.4ms`, `0.012s`, `1.2µs`) in milliseconds.
pub(crate) fn duration_ms(text: &str) -> Option<f64> {
    let t = text.trim();
    for (suffix, scale) in [("ms", 1.0), ("µs", 0.001), ("us", 0.001), ("s", 1000.0)] {
        if let Some(n) = t.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().map(|v| v * scale);
        }
    }
    None
}

/// Whether text starts like a statement, so a log line that merely contains "(1.2ms)" isn't taken for one.
pub(crate) fn starts_like_sql(text: &str) -> bool {
    let word: String =
        text.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    matches!(
        word.as_str(),
        "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "WITH"
            | "REPLACE"
            | "UPSERT"
            | "MERGE"
            | "VALUES"
            | "PRAGMA"
            | "BEGIN"
            | "COMMIT"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "EXPLAIN"
            | "SET"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_with_and_without_a_zone() {
        let (m, rest) = leading_timestamp("2026-10-15 12:00:05,120 DEBUG (0.001) SELECT 1").unwrap();
        assert!(!m.zoned);
        assert_eq!(rest.trim_start(), "DEBUG (0.001) SELECT 1");
        let (z, _) = leading_timestamp("2026-10-15 12:00:01 +0000 Started").unwrap();
        assert!(z.zoned);
        assert!(leading_timestamp("Started GET /").is_none());
    }

    #[test]
    fn durations_and_colours() {
        assert_eq!(duration_ms("12.4ms"), Some(12.4));
        assert_eq!(duration_ms("0.012s"), Some(12.0));
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[36mProduct Load (1.0ms)\u{1b}[0m"), "Product Load (1.0ms)");
    }
}
