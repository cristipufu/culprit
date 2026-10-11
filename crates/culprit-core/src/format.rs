//! Human formatting shared by checks (for finding prose) and the CLI. The UI formats from raw values.

pub fn duration(ms: f64) -> String {
    let ms = ms.max(0.0);
    // Nothing measured reads as 0 ms; cache-fast work in microseconds, as the app shows it ("3 µs", not "0.00 ms").
    if ms == 0.0 {
        return "0 ms".to_string();
    }
    if ms < 0.01 {
        return format!("{:.0} µs", (ms * 1_000.0).max(1.0));
    }
    if ms < 1.0 {
        return format!("{:.2} ms", ms);
    }
    if ms < 1_000.0 {
        return format!("{:.0} ms", ms);
    }
    let s = ms / 1_000.0;
    if s < 60.0 {
        return format!("{:.1} s", s);
    }
    let m = s / 60.0;
    if m < 60.0 {
        return format!("{:.0} min", m);
    }
    let h = m / 60.0;
    if h < 48.0 {
        let whole = h.floor();
        let minutes = ((h - whole) * 60.0).round();
        if minutes >= 1.0 {
            return format!("{whole:.0} h {minutes:.0} min");
        }
        return format!("{whole:.0} h");
    }
    format!("{:.1} days", h / 24.0)
}

/// Seconds since the epoch for an RFC 3339 / ISO 8601 timestamp (UTC offsets honoured), without a date library.
pub fn epoch_seconds(text: &str) -> Option<f64> {
    let t = text.trim();
    let (date, rest) = t.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (time, offset) = if let Some(stripped) = rest.strip_suffix('Z') {
        (stripped, 0.0)
    } else if let Some(pos) = rest.rfind(['+', '-']).filter(|p| *p >= 5) {
        let (time, off) = rest.split_at(pos);
        let sign = if off.starts_with('-') { -1.0 } else { 1.0 };
        let mut parts = off[1..].split(':');
        let h: f64 = parts.next().and_then(|v| v.get(..2)).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let mm: f64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        (time, sign * (h * 3600.0 + mm * 60.0))
    } else {
        (rest, 0.0)
    };
    let mut tp = time.split(':');
    let hour: f64 = tp.next()?.parse().ok()?;
    let minute: f64 = tp.next().unwrap_or("0").parse().ok()?;
    let second: f64 = tp.next().unwrap_or("0").parse().ok()?;
    // Days from 1970-01-01 (Howard Hinnant's civil-from-days, inverted).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + hour * 3600.0 + minute * 60.0 + second - offset)
}

/// "6.2 days" between two timestamps, when both parse and `to` is not earlier.
pub fn age_between(from: &str, to: &str) -> Option<String> {
    let seconds = epoch_seconds(to)? - epoch_seconds(from)?;
    (seconds >= 0.0).then(|| duration(seconds * 1000.0))
}

pub fn bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = value.max(0.0);
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 || v >= 100.0 {
        return format!("{:.0} {}", v, UNITS[unit]);
    }
    format!("{:.1} {}", v, UNITS[unit])
}

pub fn count(value: f64) -> String {
    // An empty f64 sum is -0.0; it reads as nothing, not as a negative.
    let value = if value == 0.0 { 0.0 } else { value };
    let v = value.abs();
    if v >= 1e9 {
        return format!("{:.1}B", value / 1e9);
    }
    if v >= 1e6 {
        return format!("{:.1}M", value / 1e6);
    }
    if v >= 1e4 {
        return format!("{:.0}K", value / 1e3);
    }
    if v >= 1e3 {
        return format!("{:.1}K", value / 1e3);
    }
    if v >= 100.0 || v.fract() == 0.0 {
        return format!("{:.0}", value);
    }
    format!("{:.1}", value)
}

pub fn percent(ratio: f64) -> String {
    let p = ratio * 100.0;
    if p > 0.0 && p < 1.0 {
        return "<1%".to_string();
    }
    format!("{:.0}%", p)
}

/// "3.4×", "710×": how many times larger, written the way the rest of the product writes it.
pub fn factor(value: f64) -> String {
    if value >= 100.0 {
        return format!("{}×", count(value));
    }
    format!("{:.1}×", value)
}

pub fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `[db].[dbo].[orders].[created_at]>=[@P1]` reads as `created_at >= @P1`; mirrors the UI's shortPredicate.
pub fn predicate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let mut cursor = &rest[start..];
        let mut last = "";
        while let Some(stripped) = cursor.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            last = &stripped[..end];
            cursor = &stripped[end + 1..];
            match cursor.strip_prefix('.') {
                Some(next) if next.starts_with('[') => cursor = next,
                _ => break,
            }
        }
        out.push_str(last);
        rest = cursor;
    }
    out.push_str(rest);
    let mut spaced = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
        if matches!(two.as_str(), ">=" | "<=" | "<>") {
            spaced.push_str(&format!(" {two} "));
            i += 2;
            continue;
        }
        if matches!(chars[i], '=' | '<' | '>') {
            spaced.push_str(&format!(" {} ", chars[i]));
        } else {
            spaced.push(chars[i]);
        }
        i += 1;
    }
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "Oct 1" (with the year when it isn't `now`'s): a date as people write it, from an ISO date or timestamp.
pub fn friendly_date(iso: &str, now: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = iso.get(..10).unwrap_or(iso).split('-');
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return iso.to_string();
    };
    let (Ok(m), Ok(d)) = (month.parse::<usize>(), day.parse::<u32>()) else { return iso.to_string() };
    let Some(name) = m.checked_sub(1).and_then(|i| MONTHS.get(i)) else { return iso.to_string() };
    if now.get(..4) == Some(year) { format!("{name} {d}") } else { format!("{name} {d}, {year}") }
}

/// [`friendly_date`] relative to today (UTC).
pub fn friendly_date_today(iso: &str) -> String {
    friendly_date(iso, &chrono::Utc::now().format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_between_timestamps() {
        assert_eq!(epoch_seconds("1970-01-02T00:00:00Z"), Some(86_400.0));
        assert_eq!(epoch_seconds("2026-10-01T02:00:00+02:00"), epoch_seconds("2026-10-01T00:00:00Z"));
        assert_eq!(age_between("2026-10-01T00:00:00Z", "2026-10-01 06:00:00").as_deref(), Some("6 h"));
        assert_eq!(age_between("2026-10-02T00:00:00Z", "2026-10-01T00:00:00Z"), None);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(0.5), "0.50 ms");
        assert_eq!(duration(250.0), "250 ms");
        assert_eq!(duration(12_300.0), "12.3 s");
        assert_eq!(duration(5.0 * 3_600_000.0 + 20.0 * 60_000.0), "5 h 20 min");
    }

    #[test]
    fn predicates_read_short() {
        assert_eq!(predicate("[shop].[dbo].[orders].[created_at]>=[@P1]"), "created_at >= @P1");
        assert_eq!(predicate("(customer_id = $1)"), "(customer_id = $1)");
    }

    #[test]
    fn bytes_and_counts() {
        assert_eq!(bytes(6.1 * 1024.0 * 1024.0 * 1024.0), "6.1 GB");
        assert_eq!(count(18_200_000.0), "18.2M");
        assert_eq!(count(1_234.0), "1.2K");
        // An empty sum of f64 is -0.0.
        assert_eq!(count(std::iter::empty::<f64>().sum()), "0");
        assert_eq!(percent(0.314), "31%");
        assert_eq!(list(&["a".into(), "b".into(), "c".into()]), "a, b and c");
    }
}
