//! Pure formatting/decision helpers for the status bar, mirroring omp's own status line
//! (docs/notes/omp-status.md §1.3, §2c): token formatter, context thresholds, title glyph.

use std::path::Path;

use deck_core::sessions::abbreviate_home;

/// omp's token formatter: `999`, `1.2K`, `23K`, `1M`, `1.5M`, `120M`, `2.5B`.
pub fn fmt_tokens(n: u64) -> String {
    fn scaled(n: u64, div: f64, suffix: &str, decimals: bool) -> String {
        let v = n as f64 / div;
        if decimals {
            let s = format!("{v:.1}");
            let s = s.strip_suffix(".0").unwrap_or(&s);
            format!("{s}{suffix}")
        } else {
            format!("{}{suffix}", v.round() as u64)
        }
    }
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => scaled(n, 1e3, "K", true),
        10_000..=999_999 => scaled(n, 1e3, "K", false),
        1_000_000..=9_999_999 => scaled(n, 1e6, "M", true),
        10_000_000..=999_999_999 => scaled(n, 1e6, "M", false),
        1_000_000_000..=9_999_999_999 => scaled(n, 1e9, "B", true),
        _ => scaled(n, 1e9, "B", false),
    }
}

/// `$7.00` (omp: `"$" + cost.toFixed(2)`).
pub fn fmt_cost(cost: f64) -> String {
    format!("${:.2}", if cost.is_finite() { cost.max(0.0) } else { 0.0 })
}

/// Escalation of the context meter (omp's `chrome/context-thresholds.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextLevel {
    Normal,
    Warn,
    High,
    Critical,
}

/// Usage as a percentage of the window; `None` without both numbers or with a zero window.
pub fn context_percent(tokens: Option<u64>, window: Option<u64>) -> Option<f64> {
    let (t, w) = (tokens?, window?);
    (w > 0).then(|| t as f64 / w as f64 * 100.0)
}

/// Thresholds are `min(50 %, 150K tok)`, `min(70 %, 270K)`, `min(90 %, 500K)` of the window.
pub fn context_level(pct: f64, window: u64) -> ContextLevel {
    let cap = |pct_limit: f64, tok_limit: f64| -> f64 {
        if window == 0 {
            pct_limit
        } else {
            pct_limit.min(tok_limit / window as f64 * 100.0)
        }
    };
    if pct >= cap(90.0, 500_000.0) {
        ContextLevel::Critical
    } else if pct >= cap(70.0, 270_000.0) {
        ContextLevel::High
    } else if pct >= cap(50.0, 150_000.0) {
        ContextLevel::Warn
    } else {
        ContextLevel::Normal
    }
}

/// `17.3%/1M`, `?%/1M`, `23K/?` — the text of omp's `context_pct` segment.
pub fn context_label(tokens: Option<u64>, window: Option<u64>) -> String {
    match (context_percent(tokens, window), window) {
        (Some(pct), Some(w)) => format!("{pct:.1}%/{}", fmt_tokens(w)),
        (None, Some(w)) => format!("?%/{}", fmt_tokens(w)),
        (_, None) => match tokens {
            Some(t) => format!("{}/?", fmt_tokens(t)),
            None => "?".to_owned(),
        },
    }
}

/// What the omp TUI is doing, read from its terminal title `π <glyph> <label>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Activity {
    /// No omp title seen (yet): nothing is claimed.
    Unknown,
    Idle,
    Busy,
    /// `!` — omp wants the user's input.
    Attention,
}

impl Activity {
    pub fn from_title(title: Option<&str>) -> Activity {
        let Some(rest) = title.and_then(|t| t.trim_start().strip_prefix('π')) else {
            return Activity::Unknown;
        };
        match rest.split_whitespace().next() {
            Some(">") => Activity::Idle,
            Some("!") => Activity::Attention,
            Some(_) => Activity::Busy,
            None => Activity::Unknown,
        }
    }

    /// True while a turn streams (gating for idle-only actions).
    pub fn is_busy(self) -> bool {
        self == Activity::Busy
    }
}

/// Model name as omp's status line shows it (`Claude Opus 5.5` → `Opus 5.5`).
pub fn short_model_name(name: &str) -> &str {
    name.strip_prefix("Claude ").unwrap_or(name)
}

/// Thinking levels in ascending order with the fill of omp's glyph (`○ ◔ ◑ ◒ ◕ ◉`).
pub fn thinking_fill(level: &str) -> Option<f32> {
    Some(match level {
        "off" => return None,
        "minimal" => 0.0,
        "low" => 0.25,
        "medium" => 0.5,
        "high" => 0.75,
        "xhigh" => 0.9,
        "max" => 1.0,
        _ => 0.5,
    })
}

/// omp's short level names (`med`, `xhigh`).
pub fn thinking_short(level: &str) -> &str {
    match level {
        "minimal" => "min",
        "medium" => "med",
        other => other,
    }
}

/// `~`-abbreviated directory, left-ellipsized to `max_chars` (omp truncates the left side).
pub fn display_path(cwd: &Path, max_chars: usize) -> String {
    ellipsize_left(&abbreviate_home(cwd), max_chars)
}

pub fn ellipsize_left(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars || max_chars < 2 {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(n - (max_chars - 1)).collect();
    format!("…{tail}")
}

/// `$ / M tokens` price for the model dropdown: `3`, `0.25`, `15`.
pub fn fmt_price(p: f64) -> String {
    if !p.is_finite() || p <= 0.0 {
        return "0".to_owned();
    }
    let s = if p >= 10.0 {
        format!("{p:.0}")
    } else {
        format!("{p:.2}")
    };
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_formatter_matches_omp() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (1_234, "1.2K"),
            (9_999, "10K"),
            (23_281, "23K"),
            (500_000, "500K"),
            (1_000_000, "1M"),
            (1_500_000, "1.5M"),
            (120_000_000, "120M"),
            (2_500_000_000, "2.5B"),
        ];
        for (n, want) in cases {
            assert_eq!(fmt_tokens(n), want, "{n}");
        }
    }

    #[test]
    fn cost_has_two_decimals() {
        assert_eq!(fmt_cost(7.0), "$7.00");
        assert_eq!(fmt_cost(0.255), "$0.26");
        assert_eq!(fmt_cost(f64::NAN), "$0.00");
    }

    #[test]
    fn context_label_variants() {
        assert_eq!(context_label(Some(173_000), Some(1_000_000)), "17.3%/1M");
        assert_eq!(context_label(None, Some(1_000_000)), "?%/1M");
        assert_eq!(context_label(Some(23_281), None), "23K/?");
        assert_eq!(context_label(None, None), "?");
        assert_eq!(context_percent(Some(5), Some(0)), None);
    }

    #[test]
    fn context_thresholds_use_the_smaller_of_percent_and_tokens() {
        // 1M window: 15 % / 27 % / 50 %.
        assert_eq!(context_level(14.9, 1_000_000), ContextLevel::Normal);
        assert_eq!(context_level(15.0, 1_000_000), ContextLevel::Warn);
        assert_eq!(context_level(27.0, 1_000_000), ContextLevel::High);
        assert_eq!(context_level(50.0, 1_000_000), ContextLevel::Critical);
        // 100K window: percentages win (50 / 70 / 90).
        assert_eq!(context_level(49.0, 100_000), ContextLevel::Normal);
        assert_eq!(context_level(50.0, 100_000), ContextLevel::Warn);
        assert_eq!(context_level(70.0, 100_000), ContextLevel::High);
        assert_eq!(context_level(95.0, 100_000), ContextLevel::Critical);
    }

    #[test]
    fn activity_from_omp_title() {
        use Activity::*;
        assert_eq!(Activity::from_title(Some("π > proj")), Idle);
        assert_eq!(Activity::from_title(Some("π ⠦ proj")), Busy);
        assert_eq!(Activity::from_title(Some("π ! proj")), Attention);
        assert_eq!(Activity::from_title(Some("π > ")), Idle);
        assert_eq!(Activity::from_title(Some("host")), Unknown);
        assert_eq!(Activity::from_title(None), Unknown);
        assert_eq!(Activity::from_title(Some("π")), Unknown);
        assert!(Busy.is_busy() && !Idle.is_busy() && !Unknown.is_busy());
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(short_model_name("Claude Opus 5.5"), "Opus 5.5");
        assert_eq!(short_model_name("Fake One"), "Fake One");
        assert_eq!(ellipsize_left("/a/b/c/d", 20), "/a/b/c/d");
        assert_eq!(ellipsize_left("/aaaa/bbbb/cccc", 8), "…bb/cccc");
        assert_eq!(thinking_short("medium"), "med");
        assert_eq!(thinking_fill("off"), None);
        assert_eq!(thinking_fill("max"), Some(1.0));
    }

    #[test]
    fn prices() {
        assert_eq!(fmt_price(3.0), "3");
        assert_eq!(fmt_price(0.25), "0.25");
        assert_eq!(fmt_price(15.0), "15");
        assert_eq!(fmt_price(0.0), "0");
        assert_eq!(fmt_price(1.5), "1.5");
    }
}
