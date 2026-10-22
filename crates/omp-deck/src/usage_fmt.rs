//! Pure text formatting for the usage chip, cards and tables.

/// Whole percent left of a window that is `used_fraction` used (omp: `max(0, round((1-f)·100))`).
pub fn left_pct(used_fraction: f64) -> u32 {
    let left = ((1.0 - used_fraction) * 100.0).round();
    if left.is_finite() && left > 0.0 {
        left.min(100.0) as u32
    } else {
        0
    }
}

/// `63% left`.
pub fn left_text(used_fraction: f64) -> String {
    format!("{}% left", left_pct(used_fraction))
}

/// `resets in 2h11m` (omp's own duration style).
pub fn resets_in_text(ms: i64) -> String {
    format!("resets in {}", deck_core::usage::format_remaining(ms))
}

/// `just now`, `12s ago`, `3m ago`, `2h ago`.
pub fn updated_ago(secs: u64) -> String {
    match secs {
        0..=4 => "just now".to_owned(),
        5..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

/// `$1,234.50`, `$12.34`, `$0.12`, `$0`.
pub fn money(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "$0".to_owned();
    }
    if v >= 1000.0 {
        let whole = crate::statusbar::group_digits(v.trunc() as u64);
        let cents = ((v.fract() * 100.0).round() as u64).min(99);
        format!("${whole}.{cents:02}")
    } else {
        format!("${v:.2}")
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// `Sat, Mar 14, 2026`.
pub fn long_date(d: deck_core::usage::Date) -> String {
    let month = MONTHS
        .get(d.month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("?");
    let wd = WEEKDAYS
        .get(d.weekday_mon0() as usize)
        .copied()
        .unwrap_or("?");
    format!("{wd}, {month} {}, {}", d.day, d.year)
}

/// `1,234 requests`, `1 request`.
pub fn requests(n: u64) -> String {
    format!(
        "{} {}",
        crate::statusbar::group_digits(n),
        if n == 1 { "request" } else { "requests" }
    )
}

/// `1,234 req` — the compact form for tight spots (stat tiles).
pub fn requests_short(n: u64) -> String {
    format!("{} req", crate::statusbar::group_digits(n))
}

/// `Mar 14, 2026`.
pub fn short_date(d: deck_core::usage::Date) -> String {
    let month = MONTHS
        .get(d.month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("?");
    format!("{month} {}, {}", d.day, d.year)
}

/// `1 active day · longest streak 1 day`.
pub fn streak_text(active_days: u32, longest_streak: u32) -> String {
    let days = |n: u32| if n == 1 { "day" } else { "days" };
    format!(
        "{active_days} active {} · longest streak {longest_streak} {}",
        days(active_days),
        days(longest_streak)
    )
}

/// Usage header status: never claims fresh data when the snapshot carries none.
pub fn header_when(fetching: bool, age_secs: Option<u64>, no_data: bool) -> String {
    match (fetching, age_secs) {
        (true, _) => "Updating…".to_owned(),
        (false, Some(s)) if no_data => format!("Checked {} · no data", updated_ago(s)),
        (false, Some(s)) => format!("Updated {}", updated_ago(s)),
        (false, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_percent_rounds_and_clamps() {
        assert_eq!(left_pct(0.37), 63);
        assert_eq!(left_pct(0.815), 19);
        assert_eq!(left_pct(1.0), 0);
        assert_eq!(left_pct(1.4), 0);
        assert_eq!(left_pct(-0.2), 100);
        assert_eq!(left_pct(f64::NAN), 0);
        assert_eq!(left_text(0.9), "10% left");
    }

    #[test]
    fn reset_text_uses_omp_style() {
        assert_eq!(resets_in_text(8_039_515), "resets in 2h13m");
        assert_eq!(resets_in_text(30_000), "resets in <1m");
    }

    #[test]
    fn age_steps() {
        assert_eq!(updated_ago(2), "just now");
        assert_eq!(updated_ago(12), "12s ago");
        assert_eq!(updated_ago(125), "2m ago");
        assert_eq!(updated_ago(7300), "2h ago");
    }

    #[test]
    fn money_and_requests() {
        assert_eq!(money(0.0), "$0");
        assert_eq!(money(0.123), "$0.12");
        assert_eq!(money(12.346), "$12.35");
        assert_eq!(money(1234.5), "$1,234.50");
        assert_eq!(requests(1), "1 request");
        let d = deck_core::usage::Date::new(2026, 3, 14);
        assert_eq!(d.map(long_date).as_deref(), Some("Sat, Mar 14, 2026"));
        assert_eq!(requests(1200), "1,200 requests");
    }

    #[test]
    fn streak_is_pluralised() {
        assert_eq!(streak_text(1, 1), "1 active day · longest streak 1 day");
        assert_eq!(streak_text(32, 2), "32 active days · longest streak 2 days");
        assert_eq!(streak_text(0, 0), "0 active days · longest streak 0 days");
    }

    #[test]
    fn header_never_claims_fresh_data_without_data() {
        assert_eq!(header_when(false, Some(1), false), "Updated just now");
        assert_eq!(
            header_when(false, Some(1), true),
            "Checked just now · no data"
        );
        assert_eq!(header_when(true, Some(1), true), "Updating…");
        assert_eq!(header_when(false, None, true), "");
    }
}
