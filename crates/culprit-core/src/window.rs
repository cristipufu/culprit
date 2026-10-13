//! The workload history window: how far back a scan reads where the engine keeps history (SQL Server Query Store,
//! the MySQL slow log). Kept in minutes, so a window can be hours (`6h`) as well as days (`7d`).

pub const MINUTES_PER_DAY: u32 = 24 * 60;
/// The longest window a scan reads: 90 days.
pub const MAX_WINDOW_MINUTES: u32 = 90 * MINUTES_PER_DAY;

/// Reads a window written as days, hours and minutes, each at most once and in that order: `6h`, `90m`, `2d`, `1d12h`.
/// Returns the window in minutes, or a sentence saying what's wrong with it.
pub fn parse_window(text: &str) -> Result<u32, String> {
    let given = text.trim().to_ascii_lowercase();
    let invalid = || format!("\"{}\" isn't a window: write it as 6h, 90m, 2d or 1d12h.", text.trim());
    if given.is_empty() {
        return Err(invalid());
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    // Days, then hours, then minutes: each unit's position, so a repeated or out-of-order unit is refused.
    let mut last_unit: Option<usize> = None;
    for c in given.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let (position, minutes) = match c {
            'd' => (0, u64::from(MINUTES_PER_DAY)),
            'h' => (1, 60),
            'm' => (2, 1),
            _ => return Err(invalid()),
        };
        if digits.is_empty() || last_unit.is_some_and(|last| position <= last) {
            return Err(invalid());
        }
        // Only digits got here, so a number that doesn't parse is one too large for any window.
        let value: u64 = digits.parse().unwrap_or(u64::MAX);
        total = total.saturating_add(value.saturating_mul(minutes));
        digits.clear();
        last_unit = Some(position);
    }
    // A bare number has no unit to say whether it's hours or days.
    if !digits.is_empty() || last_unit.is_none() {
        return Err(invalid());
    }
    if total == 0 {
        return Err(format!("\"{}\" is an empty window: give it at least a minute.", text.trim()));
    }
    if total > u64::from(MAX_WINDOW_MINUTES) {
        return Err(format!("\"{}\" is longer than the 90 days a scan reads at most.", text.trim()));
    }
    Ok(total as u32)
}

/// The shortest exact way to write a window: `7d`, `6h`, `36h`, `90m`. It names the latest-scan pointer of each window,
/// so a whole number of days keeps the name it always had (`7d`).
pub fn window_label(minutes: u32) -> String {
    if minutes > 0 && minutes.is_multiple_of(MINUTES_PER_DAY) {
        format!("{}d", minutes / MINUTES_PER_DAY)
    } else if minutes > 0 && minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_days_hours_and_minutes() {
        assert_eq!(parse_window("6h"), Ok(360));
        assert_eq!(parse_window("90m"), Ok(90));
        assert_eq!(parse_window("2d"), Ok(2 * MINUTES_PER_DAY));
        assert_eq!(parse_window("1d12h"), Ok(36 * 60));
        assert_eq!(parse_window("1h30m"), Ok(90));
        assert_eq!(parse_window(" 6H "), Ok(360));
        assert_eq!(parse_window("90d"), Ok(MAX_WINDOW_MINUTES));
    }

    #[test]
    fn refuses_what_isnt_a_window() {
        for text in ["", "6", "h", "6x", "-6h", "6h1d", "6h6h", "1.5h", "6 h", "d6"] {
            let error = parse_window(text).expect_err(text);
            assert!(error.contains("isn't a window"), "{text}: {error}");
        }
        assert!(parse_window("0h").unwrap_err().contains("empty window"));
        assert!(parse_window("91d").unwrap_err().contains("longer than the 90 days"));
        assert!(parse_window("99999999999999999999d").unwrap_err().contains("longer than"));
    }

    #[test]
    fn labels_a_window_by_its_largest_exact_unit() {
        assert_eq!(window_label(7 * MINUTES_PER_DAY), "7d");
        assert_eq!(window_label(360), "6h");
        assert_eq!(window_label(60), "1h");
        assert_eq!(window_label(36 * 60), "36h");
        assert_eq!(window_label(90), "90m");
    }
}
