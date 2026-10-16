//! Human-readable formatting for bot and CLI output.
//!
//! Amounts are rendered in token units (trimmed of trailing zeros), timestamps
//! in UTC plus a relative hint ("in 2h 05m" / "1h 10m ago"). No chrono: the
//! UTC civil-date conversion uses Howard Hinnant's days-to-civil algorithm.

/// Escape text for Telegram's HTML parse mode (`&`, `<`, `>`; dynamic text is
/// never placed inside attributes, so quotes stay readable).
pub fn html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Compact duration: `"45s"`, `"5m"`, `"2h 05m"`, `"3d 4h"`.
pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600),
    }
}

/// Render a wei amount in token units with `decimals` fractional digits,
/// trimming trailing zeros: `50_000_000_000_000_000_000` with 18 → `"50"`,
/// `1_500_000` with 6 → `"1.5"`.
pub fn amount(wei: u128, decimals: u8) -> String {
    // Config validation bounds decimals to <= 38, the largest u128 power of ten.
    let base = 10u128.pow(u32::from(decimals.min(38)));
    let whole = wei / base;
    let frac = wei % base;
    if frac == 0 {
        return whole.to_string();
    }
    let mut s = format!("{whole}.{frac:0width$}", width = usize::from(decimals));
    while s.ends_with('0') {
        s.pop();
    }
    s
}

/// Unix seconds → `"2026-08-11 14:23 UTC"`.
pub fn utc(ts: u64) -> String {
    let days = i64::try_from(ts / 86_400).unwrap_or(i64::MAX / 2);
    let secs_of_day = ts % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60
    )
}

/// Relative time vs `now`, e.g. `"in 2h 05m"` or `"1h 10m ago"`.
pub fn relative(ts: u64, now: u64) -> String {
    if ts <= now {
        match now - ts {
            0..60 => "just now".to_string(),
            d => format!("{} ago", duration(d)),
        }
    } else {
        match ts - now {
            0..60 => "in <1m".to_string(),
            d => format!("in {}", duration(d)),
        }
    }
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    // Both values are small by construction (1..=12, 1..=31).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_human_readable() {
        assert_eq!(amount(50_000_000_000_000_000_000, 18), "50");
        assert_eq!(amount(1, 18), "0.000000000000000001");
        assert_eq!(amount(1_500_000, 6), "1.5");
        assert_eq!(amount(1_234_567, 6), "1.234567");
        assert_eq!(amount(0, 18), "0");
        assert_eq!(amount(100, 2), "1");
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(utc(1_786_448_605), "2026-08-11 11:43 UTC");
    }

    #[test]
    fn relative_time() {
        let now = 1_000_000;
        assert_eq!(relative(now + 30, now), "in <1m");
        assert_eq!(relative(now + 5 * 60, now), "in 5m");
        assert_eq!(relative(now + 7_500, now), "in 2h 05m");
        assert_eq!(relative(now - 7_500, now), "2h 05m ago");
        assert_eq!(relative(now - 30, now), "just now");
        assert_eq!(relative(now - 2 * 86_400, now), "2d 0h ago");
    }

    #[test]
    fn html_and_durations() {
        assert_eq!(
            html("<a href=\"x\">&</a>"),
            "&lt;a href=\"x\"&gt;&amp;&lt;/a&gt;"
        );
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(3600), "1h 00m");
        assert_eq!(duration(90_000), "1d 1h");
    }
}
