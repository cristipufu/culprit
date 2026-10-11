//! Human formatting shared by checks (for finding prose) and the CLI. The UI formats from raw values.

pub fn duration(ms: f64) -> String {
    let ms = ms.max(0.0);
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

pub fn factor(value: f64) -> String {
    if value >= 100.0 {
        return format!("×{}", count(value));
    }
    format!("×{:.1}", value)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(percent(0.314), "31%");
        assert_eq!(list(&["a".into(), "b".into(), "c".into()]), "a, b and c");
    }
}
