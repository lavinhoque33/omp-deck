//! Newest-first ordering of one provider's models (`docs/notes/model-order.md`).
//!
//! omp exposes no release dates, so recency is inferred from the id: a version tuple, an optional
//! date suffix and a series (the leading letters). Within a provider every series' versions are
//! ranked, so an aggregator lists each vendor's latest model before anyone's second latest.

use std::cmp::Reverse;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    ver: Vec<u32>,
    date: Option<u32>,
    series: String,
}

/// Key of one model, comparable with `Ord` once `rank` is filled in.
type Key = (
    u32,
    bool,
    Reverse<Vec<u32>>,
    bool,
    Reverse<u32>,
    String,
    String,
);

fn is_sep(b: u8) -> bool {
    matches!(b, b'-' | b'_' | b':')
}

fn digits(b: &[u8], from: usize) -> usize {
    b[from..].iter().take_while(|c| c.is_ascii_digit()).count()
}

fn num(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |a, c| {
        a.saturating_mul(10).saturating_add(u32::from(c - b'0'))
    })
}

/// `-YYYY-MM-DD`, `-YYYYMMDD` or `-MMDD` at the end; returns the date and the remaining length.
fn strip_date(s: &str) -> (Option<u32>, usize) {
    let b = s.as_bytes();
    let n = b.len();
    let all = |r: &[u8]| r.iter().all(u8::is_ascii_digit);
    // -YYYY-MM-DD
    if n >= 11
        && b[n - 11] == b'-'
        && b[n - 6] == b'-'
        && b[n - 3] == b'-'
        && all(&b[n - 10..n - 6])
        && all(&b[n - 5..n - 3])
        && all(&b[n - 2..])
    {
        let v = num(&b[n - 10..n - 6]) * 10_000 + num(&b[n - 5..n - 3]) * 100 + num(&b[n - 2..]);
        return (Some(v), n - 11);
    }
    // -YYYYMMDD
    if n >= 9 && b[n - 9] == b'-' && all(&b[n - 8..]) {
        return (Some(num(&b[n - 8..])), n - 9);
    }
    // -MMDD: no year, so it sorts below any full date.
    if n >= 5 && b[n - 5] == b'-' && all(&b[n - 4..]) {
        let (m, d) = (num(&b[n - 4..n - 2]), num(&b[n - 2..]));
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            return (Some(m * 100 + d), n - 5);
        }
    }
    (None, n)
}

/// `27b`, `122b`, `7m`: a parameter count, not a version.
fn is_size_token(b: &[u8], start: usize) -> bool {
    let mut i = start + digits(b, start);
    if b.get(i) == Some(&b'.') {
        let f = digits(b, i + 1);
        if f > 0 {
            i += 1 + f;
        }
    }
    matches!(b.get(i), Some(b'b' | b'm')) && b.get(i + 1).is_none_or(|&c| is_sep(c))
}

fn version(s: &str) -> Vec<u32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let run = digits(b, i);
        if is_size_token(b, i) {
            i += run;
            continue;
        }
        // 1.2 / 1.2.3
        let mut parts = vec![num(&b[i..i + run])];
        let mut j = i + run;
        while b.get(j) == Some(&b'.') && digits(b, j + 1) > 0 && parts.len() < 3 {
            let d = digits(b, j + 1);
            parts.push(num(&b[j + 1..j + 1 + d]));
            j += 1 + d;
        }
        if parts.len() > 1 {
            return trim_zeros(parts);
        }
        // 4-5 (only after a separator, minor part ends at a separator/end)
        if run <= 2 && i > 0 && is_sep(b[i - 1]) && b.get(i + run) == Some(&b'-') {
            let d = digits(b, i + run + 1);
            if (1..=2).contains(&d) && b.get(i + run + 1 + d).is_none_or(|&c| is_sep(c)) {
                let minor = num(&b[i + run + 1..i + run + 1 + d]);
                return trim_zeros(vec![parts[0], minor]);
            }
        }
        return trim_zeros(parts);
    }
    Vec::new()
}

fn trim_zeros(mut v: Vec<u32>) -> Vec<u32> {
    while v.len() > 1 && v.last() == Some(&0) {
        v.pop();
    }
    v
}

fn parse(id: &str) -> Parsed {
    let lower = id.to_lowercase();
    let seg = lower.rsplit('/').next().unwrap_or(&lower);
    let (date, keep) = strip_date(seg);
    let body = &seg[..keep];
    let series = body
        .chars()
        .skip_while(|c| !c.is_ascii_alphabetic())
        .take_while(char::is_ascii_alphabetic)
        .collect();
    Parsed {
        ver: version(body),
        date,
        series,
    }
}

/// Indices into `models` (`(id, name)` pairs of one provider), newest first.
pub fn order(models: &[(&str, &str)]) -> Vec<usize> {
    let parsed: Vec<Parsed> = models.iter().map(|(id, _)| parse(id)).collect();
    let mut versions: std::collections::HashMap<&str, Vec<&Vec<u32>>> = Default::default();
    for p in parsed.iter().filter(|p| !p.ver.is_empty()) {
        versions.entry(&p.series).or_default().push(&p.ver);
    }
    for v in versions.values_mut() {
        v.sort_by(|a, b| b.cmp(a));
        v.dedup();
    }
    let key = |i: usize| -> Key {
        let p = &parsed[i];
        let rank = if p.ver.is_empty() {
            u32::MAX
        } else {
            versions
                .get(p.series.as_str())
                .and_then(|v| v.iter().position(|x| **x == p.ver))
                .map_or(u32::MAX, |r| r as u32)
        };
        (
            rank,
            p.ver.is_empty(),
            Reverse(p.ver.clone()),
            p.date.is_some(),
            Reverse(p.date.unwrap_or(0)),
            models[i].0.to_lowercase(),
            models[i].1.to_lowercase(),
        )
    };
    let mut keyed: Vec<(Key, usize)> = (0..models.len()).map(|i| (key(i), i)).collect();
    keyed.sort();
    keyed.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: &[&str] = &[
        "claude-haiku-5-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        "claude-opus-4-5",
        "claude-sonnet-4-5",
        "claude-opus-4-5-20251101",
        "claude-haiku-4-5-20251001",
        "claude-sonnet-4-5-20250929",
        "claude-opus-4-1",
        "claude-opus-4-1-20250805",
        "claude-opus-4-0",
        "claude-sonnet-4-0",
        "claude-opus-4-20250514",
        "claude-sonnet-4-20250514",
        "claude-3-5-sonnet-20241022",
        "claude-3-5-sonnet-20240620",
        "claude-3-haiku-20240307",
    ];
    const CODEX: &[&str] = &[
        "gpt-6.1-sol",
        "gpt-6-astra",
        "gpt-6-luna",
        "gpt-6-sol",
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
    ];
    const COMMANDCODE: &[&str] = &[
        "gpt-6.1-sol",
        "gpt-6-sol",
        "claude-haiku-5-5",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
        "Qwen/Qwen3.8-Max",
        "Qwen/Qwen3.8-Max-0902",
        "Qwen/Qwen3.8-27B",
        "Qwen/Qwen3.7-Max",
        "google/gemini-3.8-flash",
        "google/gemini-3.5-flash",
        "google/gemini-4-argon",
        "moonshotai/Kimi-K3",
        "moonshotai/Kimi-K2.7-Code",
        "deepseek/deepseek-v4.1-flash",
        "deepseek/deepseek-v4-pro",
        "meta/muse-spark-1.3",
        "meta/muse-spark-open",
        "zai-org/GLM-5.3",
        "zai-org/GLM-5",
        "xai/grok-4.7",
        "xai/grok-4.6",
    ];
    const OPENCODE_GO: &[&str] = &[
        "glm-5.3",
        "glm-5.2",
        "kimi-k3",
        "kimi-k2.7-code",
        "qwen3.8-max",
        "qwen3.7-max",
        "longcat-2.5-preview-free",
        "longcat-2.0",
        "minimax-m3",
        "minimax-m2.7",
        "mimo-v2.6-pro",
        "mimo-v2.5-pro",
        "hy4-preview",
        "hy3",
        "muse-spark-1.3-contributor",
        "omen-alpha",
    ];

    /// Position of `id` after ordering `ids` (the input order is deliberately reversed).
    fn pos(ids: &[&str], id: &str) -> usize {
        let rev: Vec<&str> = ids.iter().rev().copied().collect();
        let models: Vec<(&str, &str)> = rev.iter().map(|i| (*i, *i)).collect();
        let ord = order(&models);
        ord.iter()
            .position(|&i| rev[i] == id)
            .unwrap_or_else(|| panic!("{id} missing"))
    }

    #[test]
    fn newest_sorts_first_for_every_documented_pair() {
        let table: &[(&[&str], &str, &str)] = &[
            (ANTHROPIC, "claude-opus-5-5", "claude-opus-4-8"),
            (ANTHROPIC, "claude-opus-4-8", "claude-opus-4-7"),
            (ANTHROPIC, "claude-sonnet-5-5", "claude-sonnet-5"),
            (ANTHROPIC, "claude-sonnet-5", "claude-sonnet-4-6"),
            (ANTHROPIC, "claude-sonnet-4-5", "claude-sonnet-4-5-20250929"),
            (
                ANTHROPIC,
                "claude-opus-4-5-20251101",
                "claude-haiku-4-5-20251001",
            ),
            (
                ANTHROPIC,
                "claude-haiku-4-5-20251001",
                "claude-sonnet-4-5-20250929",
            ),
            (ANTHROPIC, "claude-opus-4-1", "claude-opus-4-0"),
            (ANTHROPIC, "claude-opus-4-0", "claude-opus-4-20250514"),
            (
                ANTHROPIC,
                "claude-opus-4-20250514",
                "claude-3-5-sonnet-20241022",
            ),
            (
                ANTHROPIC,
                "claude-3-5-sonnet-20241022",
                "claude-3-5-sonnet-20240620",
            ),
            (
                ANTHROPIC,
                "claude-3-5-sonnet-20240620",
                "claude-3-haiku-20240307",
            ),
            (ANTHROPIC, "claude-fable-5-1", "claude-fable-5"),
            (ANTHROPIC, "claude-haiku-5-5", "claude-fable-5-1"),
            (CODEX, "gpt-6.1-sol", "gpt-6-sol"),
            (CODEX, "gpt-6-sol", "gpt-5.6-sol"),
            (CODEX, "gpt-5.6-sol", "gpt-5.6-terra"),
            (COMMANDCODE, "gpt-5.5", "gpt-5.4"),
            (COMMANDCODE, "gpt-5.4", "gpt-5.4-mini"),
            (COMMANDCODE, "gpt-5.4-mini", "gpt-5.3-codex"),
            (COMMANDCODE, "Qwen/Qwen3.8-Max", "Qwen/Qwen3.8-Max-0902"),
            (COMMANDCODE, "Qwen/Qwen3.8-Max-0902", "Qwen/Qwen3.7-Max"),
            (
                COMMANDCODE,
                "google/gemini-3.8-flash",
                "google/gemini-3.5-flash",
            ),
            (
                COMMANDCODE,
                "google/gemini-4-argon",
                "google/gemini-3.8-flash",
            ),
            (
                COMMANDCODE,
                "moonshotai/Kimi-K3",
                "moonshotai/Kimi-K2.7-Code",
            ),
            (
                COMMANDCODE,
                "deepseek/deepseek-v4.1-flash",
                "deepseek/deepseek-v4-pro",
            ),
            (COMMANDCODE, "meta/muse-spark-1.3", "meta/muse-spark-open"),
            (COMMANDCODE, "zai-org/GLM-5.3", "zai-org/GLM-5"),
            (COMMANDCODE, "gpt-6.1-sol", "claude-haiku-5-5"),
            (COMMANDCODE, "claude-haiku-5-5", "gpt-6-sol"),
            (COMMANDCODE, "xai/grok-4.7", "xai/grok-4.6"),
            (OPENCODE_GO, "glm-5.3", "glm-5.2"),
            (OPENCODE_GO, "kimi-k3", "kimi-k2.7-code"),
            (OPENCODE_GO, "qwen3.8-max", "qwen3.7-max"),
            (OPENCODE_GO, "longcat-2.5-preview-free", "longcat-2.0"),
            (OPENCODE_GO, "minimax-m3", "minimax-m2.7"),
            (OPENCODE_GO, "mimo-v2.6-pro", "mimo-v2.5-pro"),
            (OPENCODE_GO, "hy4-preview", "hy3"),
            (OPENCODE_GO, "muse-spark-1.3-contributor", "omen-alpha"),
        ];
        // The note announces 38 rows; its table actually lists 39 (14 + 3 + 14 + 8).
        assert_eq!(table.len(), 39);
        for (ids, a, b) in table {
            assert!(pos(ids, a) < pos(ids, b), "{a} should sort before {b}");
        }
    }

    #[test]
    fn parses_versions_dates_and_sizes() {
        assert_eq!(parse("claude-opus-4-5-20251101").ver, [4, 5]);
        assert_eq!(parse("claude-opus-4-5-20251101").date, Some(20_251_101));
        assert_eq!(parse("a-2025-11-01").date, Some(20_251_101));
        assert_eq!(parse("claude-opus-4-0").ver, [4]);
        assert_eq!(parse("Qwen/Qwen3.8-27B").ver, [3, 8]);
        assert_eq!(parse("ornith-1.5-122b").ver, [1, 5]);
        assert_eq!(parse("x-27b-v2").ver, [2]);
        assert_eq!(parse("kimi-k2.7-code").series, "kimi");
        assert_eq!(parse("Qwen3.8-Max-0902").date, Some(902));
        assert!(parse("inkling").ver.is_empty());
        assert!(parse("beam:free").ver.is_empty());
    }

    #[test]
    fn order_is_total_and_ties_fall_back_to_id_then_name() {
        let models = [("b", "same"), ("a", "z"), ("a", "y")];
        assert_eq!(order(&models), [2, 1, 0]);
        assert!(order(&[]).is_empty());
    }
}
