//! Plain-text URL detection for Ctrl+click, hover and the context menu. omp only emits OSC 8
//! hyperlinks when the tmux between it and us forwards them (tmux ≥ 3.4), so visible
//! `http(s)://` / `mailto:` text must be clickable on its own.

use std::ops::Range;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;

/// Recognised schemes: exactly the ones the app may open.
const SCHEMES: &[&str] = &["https://", "http://", "mailto:"];

/// Rows followed above and below the hit row through soft wraps.
const MAX_WRAP_ROWS: i32 = 32;

/// A URL found in the grid: its text and first/last cell (grid coordinates, inclusive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UrlHit {
    pub uri: String,
    pub start: Point,
    pub end: Point,
}

/// The URL at `point`, searched across the soft-wrapped logical line containing it.
pub(crate) fn url_at<T>(term: &Term<T>, point: Point) -> Option<UrlHit> {
    let grid = term.grid();
    let last = grid.last_column();
    let wraps = |line: i32| grid[Line(line)][last].flags.contains(Flags::WRAPLINE);
    let (top_limit, bottom_limit) = (grid.topmost_line().0, grid.bottommost_line().0);
    let mut top = point.line.0;
    while top > top_limit && point.line.0 - top < MAX_WRAP_ROWS && wraps(top - 1) {
        top -= 1;
    }
    let mut bottom = point.line.0;
    while bottom < bottom_limit && bottom - point.line.0 < MAX_WRAP_ROWS && wraps(bottom) {
        bottom += 1;
    }

    let cols = grid.columns();
    let rows = (bottom - top + 1) as usize;
    let mut text = Vec::with_capacity(rows * cols);
    let mut points = Vec::with_capacity(rows * cols);
    let mut at = None;
    for line in top..=bottom {
        let row = &grid[Line(line)];
        for col in 0..cols {
            let p = Point::new(Line(line), Column(col));
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                // The spacer belongs to the wide char before it.
                if p == point {
                    at = text.len().checked_sub(1);
                }
                continue;
            }
            if p == point {
                at = Some(text.len());
            }
            text.push(cell.c);
            points.push(p);
        }
    }
    let span = url_span(&text, at?)?;
    Some(UrlHit {
        uri: text[span.clone()].iter().collect(),
        start: points[span.start],
        end: points[span.end - 1],
    })
}

/// Char range of the URL in `text` that covers index `at`.
pub(crate) fn url_span(text: &[char], at: usize) -> Option<Range<usize>> {
    let mut i = 0;
    while i < text.len() && i <= at {
        let Some(len) = scheme_at(text, i) else {
            i += 1;
            continue;
        };
        let body = i + len;
        let mut end = body;
        while end < text.len() && is_url_char(text[end]) {
            end += 1;
        }
        let end = trim_end(&text[body..end]) + body;
        if end == body {
            i = body;
            continue;
        }
        if (i..end).contains(&at) {
            return Some(i..end);
        }
        i = end;
    }
    None
}

/// Length of the scheme starting at `i`, if one does and it isn't the tail of a longer word.
fn scheme_at(text: &[char], i: usize) -> Option<usize> {
    if i > 0 && (text[i - 1].is_alphanumeric() || matches!(text[i - 1], '+' | '-' | '.')) {
        return None;
    }
    SCHEMES.iter().find_map(|s| {
        let n = s.chars().count();
        let hit = text.len() >= i + n
            && s.chars()
                .zip(&text[i..i + n])
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        hit.then_some(n)
    })
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(
            c,
            '<' | '>' | '"' | '`' | '{' | '|' | '}' | '\\' | '^' | '⟨' | '⟩' | '…'
        )
        // Box drawing and block elements: table borders and frames around the URL.
        && !('\u{2500}'..='\u{259F}').contains(&c)
}

/// Length of `body` without trailing punctuation and unbalanced closing brackets.
fn trim_end(body: &[char]) -> usize {
    let mut end = body.len();
    while end > 0 {
        let unbalanced = |open: char, close: char| {
            let s = &body[..end];
            s.iter().filter(|&&c| c == close).count() > s.iter().filter(|&&c| c == open).count()
        };
        let drop = match body[end - 1] {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' => true,
            ')' => unbalanced('(', ')'),
            ']' => unbalanced('[', ']'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(s: &str, at: usize) -> Option<String> {
        let text: Vec<char> = s.chars().collect();
        url_span(&text, at).map(|r| text[r].iter().collect())
    }

    #[test]
    fn url_at_follows_soft_wraps_but_not_hard_newlines() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::Config;
        use alacritty_terminal::term::test::TermSize;
        use alacritty_terminal::vte::ansi::Processor;

        let mut term = Term::new(Config::default(), &TermSize::new(10, 6), VoidListener);
        let mut parser: Processor = Processor::new();
        // 10 columns: "go https:/" | "/ab.io/xyz" (soft wraps) | " ok" then a hard newline.
        parser.advance(&mut term, b"go https://ab.io/xyz ok\r\nhttp://c.io");
        let at = |line: i32, col: usize| Point::new(Line(line), Column(col));
        let hit = url_at(&term, at(1, 3)).expect("url on the wrapped row");
        assert_eq!(hit.uri, "https://ab.io/xyz");
        assert_eq!((hit.start, hit.end), (at(0, 3), at(1, 9)));
        assert_eq!(url_at(&term, at(0, 5)), Some(hit));
        assert_eq!(url_at(&term, at(2, 2)), None, "\" ok\" after the URL");
        assert_eq!(
            url_at(&term, at(3, 0)).map(|h| h.uri).as_deref(),
            Some("http://c.io"),
            "the next line starts a new logical line"
        );
    }

    #[test]
    fn finds_url_under_index_only() {
        let s = "see https://a.io/x and http://b.io";
        assert_eq!(find(s, 4).as_deref(), Some("https://a.io/x"));
        assert_eq!(find(s, 17).as_deref(), Some("https://a.io/x"));
        assert_eq!(find(s, 18), None, "the space after the URL");
        assert_eq!(find(s, 30).as_deref(), Some("http://b.io"));
        assert_eq!(find(s, 1), None);
    }

    #[test]
    fn trims_enclosing_punctuation_but_keeps_balanced_brackets() {
        let s = "habitat (https://scl.so/tern).";
        assert_eq!(find(s, 12).as_deref(), Some("https://scl.so/tern"));
        let s = "https://en.wikipedia.org/wiki/Rust_(language), next";
        assert_eq!(
            find(s, 0).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
        let s = "'https://a.io/?q=1'!";
        assert_eq!(find(s, 3).as_deref(), Some("https://a.io/?q=1"));
    }

    #[test]
    fn stops_at_table_borders_and_ellipsis() {
        let s = "│ https://jira.example/browse/IVN-24148│ x";
        assert_eq!(
            find(s, 5).as_deref(),
            Some("https://jira.example/browse/IVN-24148")
        );
        let s = "https://truncated.example/pa… │";
        assert_eq!(find(s, 0).as_deref(), Some("https://truncated.example/pa"));
    }

    #[test]
    fn rejects_bare_schemes_and_embedded_prefixes() {
        assert_eq!(find("https:// x", 2), None);
        assert_eq!(find("xhttps://a.io", 3), None);
        assert_eq!(find("git+https://a.io", 6), None);
        assert_eq!(
            find("HTTPS://A.IO/x", 0).as_deref(),
            Some("HTTPS://A.IO/x"),
            "schemes are case-insensitive"
        );
        assert_eq!(
            find("mail mailto:me@a.io.", 8).as_deref(),
            Some("mailto:me@a.io")
        );
    }
}
