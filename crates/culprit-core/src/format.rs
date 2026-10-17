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
    // A tenth under 10 ms: rounding 2.59 ms to "3 ms" is off by 16%. A whole number reads without its ".0", and one
    // that rounds up to 10 reads as 10 ms.
    if ms < 9.95 {
        let tenths = format!("{ms:.1}");
        return format!("{} ms", tenths.strip_suffix(".0").unwrap_or(&tenths));
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

/// Days counted, never more than were: a tenth under 10 (9.51 days are "9.5", not "10"), whole days from 10 on.
pub fn days_counted(days: f64) -> String {
    // A hair under a whole number from adding hours is that number.
    let days = days + 1e-9;
    if days < 10.0 {
        let tenths = format!("{:.1}", (days * 10.0).floor() / 10.0);
        tenths.strip_suffix(".0").unwrap_or(&tenths).to_string()
    } else {
        format!("{}", days.floor())
    }
}

/// A span of days as a metric says it: [`days_counted`] ("9.5 days", "1 day"), and under a day the hours and minutes
/// [`duration`] gives ("9 h 36 min"), never "0 days".
pub fn days(days: f64) -> String {
    if days < 1.0 {
        return duration(days.max(0.0) * 86_400_000.0);
    }
    match days_counted(days).as_str() {
        "1" => "1 day".to_string(),
        n => format!("{n} days"),
    }
}

/// Seconds since the epoch for an RFC 3339 / ISO 8601 timestamp (UTC offsets honoured), without a date library.
pub fn epoch_seconds(text: &str) -> Option<f64> {
    let t = text.trim();
    let (date, rest) = t.split_once(['T', ' '])?;
    let mut d = date.split('-');
    // Real calendar ranges only: timestamps come from databases and saved reports, and a year near i64::MAX would
    // overflow the day arithmetic below.
    let y: i64 = d.next()?.parse().ok().filter(|y| (1..=9999).contains(y))?;
    let m: i64 = d.next()?.parse().ok().filter(|m| (1..=12).contains(m))?;
    let day: i64 = d.next()?.parse().ok().filter(|d| (1..=31).contains(d))?;
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
    let mut tp = time.trim().split(':');
    let in_range = |max: f64| move |v: &f64| (0.0..=max).contains(v);
    let hour: f64 = tp.next()?.parse().ok().filter(in_range(24.0))?;
    let minute: f64 = tp.next().unwrap_or("0").parse().ok().filter(in_range(59.0))?;
    let second: f64 = tp.next().unwrap_or("0").parse().ok().filter(in_range(61.0))?;
    // Days from 1970-01-01 (Howard Hinnant's civil-from-days, inverted).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + hour * 3600.0 + minute * 60.0 + second - offset)
}

/// An ISO 8601 UTC timestamp (whole seconds) for seconds since 1970: [`epoch_seconds`] the other way.
pub fn iso_from_epoch(epoch: f64) -> String {
    let secs = epoch.floor() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
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

/// `[db].[dbo].[orders].[created_at]>=[@P1]` reads as `created_at >= @P1`, and MySQL's `` `shop`.`orders`.`status` ``
/// as `status`; mirrors the UI's shortPredicate. Prose quotes predicates in backtick code spans, so none may be left in.
pub fn predicate(text: &str) -> String {
    let out = last_names(&last_names(text, '[', ']'), '`', '`');
    let out = out.replace('`', "");
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

/// Each chain of quoted names (`[a].[b].[c]`) as its last name (`c`).
fn last_names(text: &str, open: char, close: char) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let mut cursor = &rest[start..];
        let mut last = "";
        while let Some(stripped) = cursor.strip_prefix(open) {
            let Some(end) = stripped.find(close) else { break };
            last = &stripped[..end];
            cursor = &stripped[end + close.len_utf8()..];
            match cursor.strip_prefix('.') {
                Some(next) if next.starts_with(open) => cursor = next,
                _ => break,
            }
        }
        // An opening quote that never closes stays as it is.
        if last.is_empty() && cursor.starts_with(open) {
            out.push(open);
            rest = &cursor[open.len_utf8()..];
            continue;
        }
        out.push_str(last);
        rest = cursor;
    }
    out.push_str(rest);
    out
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

    #[test]
    fn epoch_round_trips_through_iso() {
        for t in ["1970-01-01T00:00:00Z", "2026-10-15T12:00:01Z", "2000-02-29T23:59:59Z", "2100-03-01T00:00:00Z"] {
            assert_eq!(iso_from_epoch(epoch_seconds(t).unwrap()), t);
        }
        // Rails writes the offset after a space.
        assert_eq!(epoch_seconds("2026-10-15 14:00:01 +0200"), epoch_seconds("2026-10-15T12:00:01Z"));
    }

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

    // The aptabase readings: 2.59 ms read "3 ms" and 1.44 ms "1 ms". Under 10 ms a whole number is off by up to a
    // third, so a tenth stays; a whole value reads without ".0", and from 10 ms whole numbers are close enough.
    #[test]
    fn durations_under_ten_ms_keep_a_tenth() {
        assert_eq!(duration(2.59), "2.6 ms");
        assert_eq!(duration(1.44), "1.4 ms");
        assert_eq!(duration(5.0), "5 ms");
        assert_eq!(duration(1.0), "1 ms");
        assert_eq!(duration(9.97), "10 ms");
        assert_eq!(duration(12.4), "12 ms");
        assert_eq!(duration(0.99), "0.99 ms");
    }

    #[test]
    fn predicates_read_short() {
        assert_eq!(predicate("[shop].[dbo].[orders].[created_at]>=[@P1]"), "created_at >= @P1");
        assert_eq!(predicate("(customer_id = $1)"), "(customer_id = $1)");
        assert_eq!(predicate("(`shop`.`orders`.`status` = 'open')"), "(status = 'open')");
        assert!(!predicate("`a`.`b` = '`'").contains("`"));
        // A bracket that never closes (inside a value) neither hangs nor loses the text.
        assert_eq!(predicate("[t].[c] = N'a[b'"), "c = N'a[b'");
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

    // A staging SQL Server reading: the summary said "9.5 days" and the brief's metric "Counting for: 10 days"; the app
    // said "0 days" where the brief said hours. One rule: never more days than were counted, and hours under a day.
    #[test]
    fn days_say_what_was_counted() {
        assert_eq!(days(9.51), "9.5 days");
        assert_eq!(days(10.9), "10 days");
        assert_eq!(days(1.04), "1 day");
        assert_eq!(days(0.4), "9 h 36 min");
        assert_eq!(days(0.01), "14 min");
        assert_eq!(days(0.0), "0 ms");
        assert_eq!(days_counted(2.999_999_999_9), "3");
    }

    /// Inputs every surface formats: `app/src/lib/format.ts` must read each as this module does. Exact ties (2.5,
    /// 0.125, 12.5%) are among them: Rust rounds them to even, JavaScript's toFixed up.
    const SHARED_CASES: [(&str, &[f64]); 5] = [
        (
            "duration",
            &[
                0.0,
                0.0004,
                0.004,
                0.0125,
                0.125,
                0.5,
                0.999,
                1.0,
                1.44,
                2.5,
                2.59,
                2.65,
                9.94,
                9.95,
                9.97,
                10.5,
                12.5,
                250.0,
                999.4,
                999.5,
                1_000.0,
                1_250.0,
                12_300.0,
                59_949.0,
                59_950.0,
                60_000.0,
                150_000.0,
                3_570_000.0,
                3_600_000.0,
                5_400_000.0,
                19_200_000.0,
                172_799_000.0,
                172_800_000.0,
                820_800_000.0,
            ],
        ),
        (
            "bytes",
            &[0.0, 0.5, 2.5, 512.0, 1_023.0, 1_024.0, 1_536.0, 2_304.0, 102_400.0, 1_048_576.0, 1.5e9, 6.55e9, 9.7e7],
        ),
        (
            "count",
            &[
                -0.0,
                0.0,
                1.0,
                1.6,
                2.5,
                12.25,
                99.5,
                100.5,
                999.0,
                1_000.0,
                1_234.0,
                1_250.0,
                1_550.0,
                9_999.0,
                10_000.0,
                12_500.0,
                999_999.0,
                1_000_000.0,
                1_250_000.0,
                18_200_000.0,
                2.5e9,
                -1_250.0,
            ],
        ),
        ("percent", &[0.0, 0.001, 0.005, 0.0099, 0.01, 0.125, 0.135, 0.314, 0.5, 0.995, 1.0, 1.005]),
        ("days", &[0.0, 0.0007, 0.01, 0.4, 0.99, 1.0, 1.04, 2.96, 9.51, 9.99, 10.0, 10.9, 400.0]),
    ];

    /// What this module writes for each of [`SHARED_CASES`]: the table `format.ts` is checked against.
    fn shared_cases() -> Vec<(&'static str, Vec<(f64, String)>)> {
        SHARED_CASES
            .iter()
            .map(|(name, inputs)| {
                let f: fn(f64) -> String = match *name {
                    "duration" => duration,
                    "bytes" => bytes,
                    "count" => count,
                    "percent" => percent,
                    "days" => days,
                    other => panic!("no formatter {other}"),
                };
                (*name, inputs.iter().map(|&v| (v, f(v))).collect())
            })
            .collect()
    }

    // The app formats metrics and figures from raw values; a value must read the same there as in the finding's prose
    // and brief ("2.6 ms" both, not "3 ms" in the app). `cargo test -p culprit-core` writes the cases to
    // app/src/gen/format-cases.json, and `bun run typecheck` runs app/src/lib/format.check.ts over them.
    #[test]
    fn the_app_formats_numbers_as_this_module_does() {
        let cases = shared_cases();
        let said = |name: &str, input: f64| {
            cases
                .iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, c)| c.iter().find(|(v, _)| *v == input))
                .unwrap()
                .1
                .clone()
        };
        // Ties to even, as Rust's formatting rounds them; the app follows.
        assert_eq!(said("duration", 2.5), "2.5 ms");
        assert_eq!(said("duration", 0.125), "0.12 ms");
        assert_eq!(said("duration", 150_000.0), "2 min");
        assert_eq!(said("count", 12_500.0), "12K");
        assert_eq!(said("percent", 0.125), "12%");
        assert_eq!(said("days", 0.4), "9 h 36 min");
        if let Ok(dir) = std::env::var("TS_RS_EXPORT_DIR") {
            let map: serde_json::Map<String, serde_json::Value> =
                cases.into_iter().map(|(name, c)| (name.to_string(), serde_json::json!(c))).collect();
            let text = serde_json::to_string_pretty(&map).expect("cases") + "\n";
            std::fs::write(std::path::Path::new(&dir).join("format-cases.json"), text)
                .expect("app/src/gen/format-cases.json");
        }
    }
}
