//! One-line summaries of long (multi-line) error messages, so toasts and inline errors stay small.

/// Longest summary line, in characters (including the ellipsis).
pub const MAX_CHARS: usize = 160;

/// First meaningful line of `text`, trimmed and cut to [`MAX_CHARS`]; the flag says whether
/// anything was left out (further lines or a cut line), i.e. whether "Details" is worth offering.
///
/// Runtime stack traces open with a source excerpt (`10665 |   let z = false;`, a `^` marker);
/// those leading lines are skipped in favor of the actual error message after them.
pub fn summarize(text: &str) -> (String, bool) {
    let skipped = text != strip_context(text);
    let mut lines = strip_context(text)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default();
    let more_lines = skipped || lines.next().is_some();
    if first.chars().count() <= MAX_CHARS {
        return (first.to_owned(), more_lines);
    }
    let mut cut: String = first.chars().take(MAX_CHARS - 1).collect();
    cut.push('…');
    (cut, true)
}

/// `text` without its leading source-excerpt lines (`10665 |   code`, `^` pointers); unchanged
/// when it does not open with one. Use it before prefixing a message with a label.
pub fn strip_context(text: &str) -> &str {
    let mut rest = text;
    let mut found = false;
    while let Some((line, tail)) = rest.split_once('\n') {
        let t = line.trim();
        if is_source_context(t) && !t.is_empty() {
            found = true;
        } else if !(found && t.is_empty()) {
            break;
        }
        rest = tail;
    }
    if found && !rest.trim().is_empty() {
        rest
    } else {
        text
    }
}

/// `10665 | code` excerpt lines and bare `^` pointers.
fn is_source_context(line: &str) -> bool {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let numbered = digits > 0 && line[digits..].trim_start().starts_with('|');
    numbered || line.chars().all(|c| matches!(c, '^' | '~' | ' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_short_line_is_untouched() {
        assert_eq!(summarize("  boom  "), ("boom".to_owned(), false));
        assert_eq!(summarize(""), (String::new(), false));
    }

    #[test]
    fn multi_line_keeps_first_non_empty_line() {
        let (line, more) = summarize("\n\n  Error: bad\n    at x (y.ts:1)\n    at z");
        assert_eq!(line, "Error: bad");
        assert!(more);
    }

    #[test]
    fn source_excerpt_is_skipped() {
        let trace = "10665 |   let z = false;\n10666 |   try {\n  ^\nEACCES: permission denied, rename 'a' -> 'b'\n    path: \"a\"";
        let (line, more) = summarize(trace);
        assert_eq!(line, "EACCES: permission denied, rename 'a' -> 'b'");
        assert!(more);
    }

    #[test]
    fn long_line_is_cut_with_ellipsis() {
        let (line, more) = summarize(&"é".repeat(500));
        assert_eq!(line.chars().count(), MAX_CHARS);
        assert!(line.ends_with('…'));
        assert!(more);
    }
}
