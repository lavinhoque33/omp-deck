//! Message text helpers: XML-ish wrapper blocks (`<irc from="parent" agent="Main">…</irc>`,
//! `<system-notice>…</system-notice>`) and plain one-line previews of markdown.
//!
//! A wrapper block is recognised only where omp writes one: the opening tag alone on its line
//! (`<name attrs>`), the closing tag alone on a later line (`</name>`), outside fenced code. The
//! one-line forms `<name attrs>body</name>` and `<name attrs/>` count too. Names are lowercase
//! (`[a-z][a-z0-9_-]*`), so JSX/HTML-ish prose like `<Tooltip>` or an inline `i=<id>` stays text;
//! an opening tag without its closing line is text as well.

pub use super::scan::strip_preamble;
use super::scan::target_section;

/// One wrapper block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tagged<'a> {
    pub name: &'a str,
    /// Attribute values verbatim (no entity decoding); a bare attribute has an empty value.
    pub attrs: Vec<(&'a str, &'a str)>,
    /// Text between the tag lines.
    pub body: &'a str,
    /// The whole block, tag lines included.
    pub raw: &'a str,
}

impl Tagged<'_> {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Text outside any block; never blank.
    Text(&'a str),
    Tagged(Tagged<'a>),
}

/// `text` split into top-level wrapper blocks and the text around them (blocks nested in a body
/// stay in that body; split it again to descend).
pub fn segments(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut text_start = 0;
    let mut pos = 0;
    let mut fence = None;
    while pos < text.len() {
        let end = line_end(text, pos);
        let line = &text[pos..end];
        if !in_fence(&mut fence, line)
            && let Some(open) = open_tag(line.trim())
        {
            let found = match open.form {
                Form::Block => close_tag(text, end, open.name)
                    .map(|(body_end, block_end)| (&text[end..body_end], block_end)),
                Form::Inline(body) => Some((body, end)),
                Form::Empty => Some(("", end)),
            };
            if let Some((body, block_end)) = found {
                push_text(&mut out, &text[text_start..pos]);
                out.push(Segment::Tagged(Tagged {
                    name: open.name,
                    attrs: open.attrs,
                    body,
                    raw: &text[pos..block_end],
                }));
                text_start = block_end;
                pos = block_end;
                continue;
            }
        }
        pos = end;
    }
    push_text(&mut out, &text[text_start..]);
    out
}

/// An ATX heading (`## Title`) outside fenced code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heading<'a> {
    /// 1–6.
    pub level: u8,
    /// Heading text, closing `#`s removed; inline markdown kept.
    pub text: &'a str,
}

/// Markdown split at its headings: each section is a heading (none for text before the first)
/// and the text up to the next heading.
pub fn sections(text: &str) -> Vec<(Option<Heading<'_>>, &str)> {
    let mut out = Vec::new();
    let mut current = None;
    let mut body_start = 0;
    let mut pos = 0;
    let mut fence = None;
    while pos < text.len() {
        let end = line_end(text, pos);
        if !in_fence(&mut fence, &text[pos..end])
            && let Some(h) = heading(&text[pos..end])
        {
            if current.is_some() || !text[body_start..pos].trim().is_empty() {
                out.push((current, &text[body_start..pos]));
            }
            current = Some(h);
            body_start = end;
        }
        pos = end;
    }
    if current.is_some() || !text[body_start..].trim().is_empty() {
        out.push((current, &text[body_start..]));
    }
    out
}

fn heading(line: &str) -> Option<Heading<'_>> {
    let l = line.trim_end();
    let indent = l.len() - l.trim_start().len();
    let l = l.trim_start();
    let hashes = l.chars().take_while(|&c| c == '#').count();
    if indent > 3 || !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &l[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some(Heading {
        level: hashes as u8,
        text,
    })
}

/// The inner text of a line that is only a bracketed note, e.g. `[Wait interrupted by message]`.
pub fn bracket_note(line: &str) -> Option<&str> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    (!inner.is_empty() && !inner.contains(['[', ']', '\n'])).then_some(inner)
}

/// A clean single line for a collapsed header: the assignment preamble dropped (and only the
/// `# Target` section kept when there is one), wrapper-tag lines, bracketed notes, fenced code and
/// rules skipped, markdown syntax removed, whitespace collapsed; at most `max` chars plus `…`.
pub fn preview(text: &str, max: usize) -> String {
    let body = strip_preamble(text);
    let body = target_section(body).unwrap_or(body);
    let mut words = Words::new(max);
    let mut fence = None;
    for line in body.lines() {
        if words.full {
            break;
        }
        if in_fence(&mut fence, line) {
            continue;
        }
        let l = line.trim();
        if l.is_empty() || bracket_note(l).is_some() || is_rule(l) {
            continue;
        }
        let l = match open_tag(l) {
            Some(Open {
                form: Form::Inline(body),
                ..
            }) => body,
            Some(_) => continue,
            None if closing_name(l).is_some() => continue,
            None => l,
        };
        let (l, heading) = strip_block_marker(l);
        let before = words.chars;
        inline_plain(l, &mut words);
        if heading && words.chars > before && !words.out.ends_with([':', '.', '?', '!']) {
            words.out.push(':');
            words.chars += 1;
        }
    }
    words.out
}

// ---------------------------------------------------------------------------------------------
// Tags

enum Form<'a> {
    /// `<name>` alone: the body runs to a `</name>` line.
    Block,
    /// `<name>body</name>` on one line.
    Inline(&'a str),
    /// `<name/>`.
    Empty,
}

struct Open<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
    form: Form<'a>,
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A trimmed line that is an opening tag (see the module docs).
fn open_tag(line: &str) -> Option<Open<'_>> {
    let rest = line.strip_prefix('<')?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    if !valid_name(name) {
        return None;
    }
    let mut s = &rest[name_end..];
    let mut attrs = Vec::new();
    loop {
        s = s.trim_start();
        if let Some(after) = s.strip_prefix("/>") {
            return after.is_empty().then_some(Open {
                name,
                attrs,
                form: Form::Empty,
            });
        }
        if let Some(after) = s.strip_prefix('>') {
            let form = if after.is_empty() {
                Form::Block
            } else {
                let body = after
                    .strip_suffix('>')
                    .and_then(|a| a.strip_suffix(name))
                    .and_then(|a| a.strip_suffix("</"))?;
                Form::Inline(body)
            };
            return Some(Open { name, attrs, form });
        }
        let key_end = s
            .find(|c: char| c.is_whitespace() || matches!(c, '=' | '>' | '/' | '"' | '\'' | '<'))
            .unwrap_or(s.len());
        if key_end == 0 {
            return None;
        }
        let key = &s[..key_end];
        s = &s[key_end..];
        let value = match s.strip_prefix('=') {
            Some(v) => match v.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let close = v[1..].find(q)?;
                    s = &v[close + 2..];
                    &v[1..close + 1]
                }
                _ => {
                    let end = v
                        .find(|c: char| c.is_whitespace() || c == '>')
                        .unwrap_or(v.len());
                    if end == 0 {
                        return None;
                    }
                    s = &v[end..];
                    &v[..end]
                }
            },
            None => "",
        };
        attrs.push((key, value));
    }
}

/// The name of a trimmed `</name>` line.
fn closing_name(line: &str) -> Option<&str> {
    let name = line.strip_prefix("</")?.strip_suffix('>')?.trim_end();
    valid_name(name).then_some(name)
}

/// From byte `from` (a line start), the `</name>` line closing a block opened just before it:
/// `(start of that line, end of that line)`. Fenced code is skipped and same-name blocks nest.
fn close_tag(text: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut fence = None;
    let mut pos = from;
    while pos < text.len() {
        let end = line_end(text, pos);
        let line = &text[pos..end];
        if !in_fence(&mut fence, line) {
            let l = line.trim();
            if closing_name(l) == Some(name) {
                if depth == 0 {
                    return Some((pos, end));
                }
                depth -= 1;
            } else if let Some(Open {
                name: n,
                form: Form::Block,
                ..
            }) = open_tag(l)
                && n == name
            {
                depth += 1;
            }
        }
        pos = end;
    }
    None
}

fn line_end(text: &str, pos: usize) -> usize {
    text[pos..].find('\n').map_or(text.len(), |i| pos + i + 1)
}

fn push_text<'a>(out: &mut Vec<Segment<'a>>, text: &'a str) {
    if !text.trim().is_empty() {
        out.push(Segment::Text(text));
    }
}

/// Advances the fenced-code state over `line`; true when the line is a fence or inside one.
fn in_fence(fence: &mut Option<(char, usize)>, line: &str) -> bool {
    let l = line.trim_start();
    let run = |c: char| l.chars().take_while(|&x| x == c).count();
    match *fence {
        Some((c, n)) => {
            let r = run(c);
            if r >= n && l[r..].trim().is_empty() {
                *fence = None;
            }
            true
        }
        None => {
            for c in ['`', '~'] {
                let r = run(c);
                if r >= 3 {
                    *fence = Some((c, r));
                    return true;
                }
            }
            false
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Preview

/// Words joined by single spaces up to `max` chars, then `…`.
struct Words {
    out: String,
    chars: usize,
    max: usize,
    full: bool,
}

impl Words {
    fn new(max: usize) -> Words {
        Words {
            out: String::with_capacity(max + 4),
            chars: 0,
            max,
            full: false,
        }
    }

    fn push_text(&mut self, text: &str) {
        for word in text.split_whitespace() {
            if self.full {
                return;
            }
            let sep = usize::from(!self.out.is_empty());
            let n = word.chars().count();
            if self.chars + sep + n > self.max {
                let room = self.max.saturating_sub(self.chars + sep);
                if sep == 1 && room > 0 {
                    self.out.push(' ');
                }
                self.out.extend(word.chars().take(room));
                self.out.push('…');
                self.full = true;
                return;
            }
            if sep == 1 {
                self.out.push(' ');
            }
            self.out.push_str(word);
            self.chars += sep + n;
        }
    }
}

/// `---`, `***`, `===`, table separators.
fn is_rule(l: &str) -> bool {
    l.len() >= 3
        && l.chars()
            .all(|c| matches!(c, '-' | '*' | '_' | '=' | ' ' | '|' | ':'))
        && l.chars().any(|c| matches!(c, '-' | '*' | '_' | '='))
}

/// Leading quote / heading / list / task markers removed; true for a heading.
fn strip_block_marker(mut l: &str) -> (&str, bool) {
    let mut is_heading = false;
    loop {
        let before = l;
        if let Some(r) = l.strip_prefix('>') {
            l = r.trim_start();
        }
        if let Some(h) = heading(l) {
            l = h.text;
            is_heading = true;
        }
        for m in ["- ", "* ", "+ ", "[ ] ", "[x] ", "[X] "] {
            if let Some(r) = l.strip_prefix(m) {
                l = r.trim_start();
            }
        }
        let digits = l.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && (l[digits..].starts_with(". ") || l[digits..].starts_with(") ")) {
            l = l[digits + 2..].trim_start();
        }
        if l == before {
            return (l, is_heading);
        }
    }
}

/// Inline markdown removed: emphasis/code markers, `[text](url)` → text, table pipes → spaces.
fn inline_plain(l: &str, words: &mut Words) {
    let mut plain = String::with_capacity(l.len());
    let mut rest = l;
    while let Some(c) = rest.chars().next() {
        if c == '!' && rest[1..].starts_with('[') {
            rest = &rest[1..];
            continue;
        }
        if c == '['
            && let Some(mid) = rest.find("](")
            && let Some(close) = rest[mid..].find(')')
        {
            plain.push_str(&rest[1..mid]);
            rest = &rest[mid + close + 1..];
            continue;
        }
        match c {
            '`' | '*' => {}
            '|' => plain.push(' '),
            '_' if rest[1..].starts_with('_') => rest = &rest[1..],
            '~' if rest[1..].starts_with('~') => rest = &rest[1..],
            _ => plain.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    words.push_text(&plain);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged<'a>(s: &'a Segment<'a>) -> &'a Tagged<'a> {
        match s {
            Segment::Tagged(t) => t,
            Segment::Text(t) => panic!("expected a block, got text {t:?}"),
        }
    }

    #[test]
    fn irc_block_with_attrs_and_a_note_before_it() {
        let t = "[Wait interrupted by message]\n<irc from=\"parent\" agent='Main'>\nFYI: **edited** `x.rs`.\n</irc>";
        let segs = segments(t);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], Segment::Text("[Wait interrupted by message]\n"));
        assert_eq!(
            bracket_note("[Wait interrupted by message]\n"),
            Some("Wait interrupted by message")
        );
        let irc = tagged(&segs[1]);
        assert_eq!(irc.name, "irc");
        assert_eq!(irc.attr("from"), Some("parent"));
        assert_eq!(irc.attr("agent"), Some("Main"));
        assert_eq!(irc.attr("missing"), None);
        assert_eq!(irc.body, "FYI: **edited** `x.rs`.\n");
        assert!(irc.raw.starts_with("<irc ") && irc.raw.ends_with("</irc>"));
    }

    #[test]
    fn several_blocks_with_text_around_them() {
        let t = "intro\n<irc agent=\"A\">\none\n</irc>\nmiddle\n<system-reminder>\nkeep going\n</system-reminder>\n<meta lines=\"3\" />\n<notice>job done</notice>\noutro";
        let segs = segments(t);
        let shape: Vec<String> = segs
            .iter()
            .map(|s| match s {
                Segment::Text(t) => format!("text:{}", t.trim()),
                Segment::Tagged(t) => format!("{}:{}", t.name, t.body.trim()),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "text:intro",
                "irc:one",
                "text:middle",
                "system-reminder:keep going",
                "meta:",
                "notice:job done",
                "text:outro"
            ]
        );
        assert_eq!(tagged(&segs[4]).attr("lines"), Some("3"));
    }

    #[test]
    fn nested_blocks_stay_in_the_outer_body() {
        let t = "<system-notice>\nDone.\n<task-result id=\"X\">\n<output>\n{}\n</output>\n</task-result>\n</system-notice>";
        let segs = segments(t);
        assert_eq!(segs.len(), 1);
        let outer = tagged(&segs[0]);
        assert_eq!(outer.name, "system-notice");
        let inner = segments(outer.body);
        assert_eq!(inner[0], Segment::Text("Done.\n"));
        assert_eq!(tagged(&inner[1]).attr("id"), Some("X"));
        assert_eq!(tagged(&segments(tagged(&inner[1]).body)[0]).name, "output");
    }

    #[test]
    fn tags_inside_code_fences_are_text() {
        let t = "See:\n```xml\n<irc from=\"parent\">\nnot a message\n</irc>\n```\nend";
        assert_eq!(segments(t), vec![Segment::Text(t)]);
        // A closing line inside a fence in the body does not close the block.
        let t = "<context>\n~~~\n</context>\n~~~\nreal\n</context>\nafter";
        let segs = segments(t);
        assert_eq!(tagged(&segs[0]).body, "~~~\n</context>\n~~~\nreal\n");
        assert_eq!(segs[1], Segment::Text("after"));
    }

    #[test]
    fn unclosed_and_non_wrapper_tags_are_text() {
        for t in [
            "<irc from=\"parent\">\nbody never closed",
            "<Tooltip>\nx\n</Tooltip>",
            "inline <irc>x</irc> mention",
            "<irc from=\"unterminated>\nx\n</irc>",
            "<div class=>\nx\n</div>",
        ] {
            assert_eq!(segments(t), vec![Segment::Text(t)], "{t:?}");
        }
    }

    #[test]
    fn sections_split_at_headings_outside_fences() {
        let t = "intro\n# Target\nDo it.\n```sh\n# not a heading\n```\n## Notes ##\n#hashtag text\n###\n";
        let h = |level, text| Some(Heading { level, text });
        assert_eq!(
            sections(t),
            vec![
                (None, "intro\n"),
                (h(1, "Target"), "Do it.\n```sh\n# not a heading\n```\n"),
                (h(2, "Notes"), "#hashtag text\n"),
                (h(3, ""), ""),
            ]
        );
        assert_eq!(sections("plain"), vec![(None, "plain")]);
    }

    #[test]
    fn preview_is_plain_text() {
        let a = "Complete assignment thoroughly:\n\n# Goal\nignored\n# Target\nInline **image** previews in `deck-term`; see [docs](https://x.y/z).\n```rust\nfn hidden() {}\n```\n- list item\n# Change\nsteps";
        assert_eq!(
            preview(a, 200),
            "Inline image previews in deck-term; see docs. list item"
        );
        let irc = "[Wait interrupted by message]\n<irc from=\"parent\" agent=\"Main\">\n## Heads up\nstop now\n</irc>";
        assert_eq!(preview(irc, 200), "Heads up: stop now");
        assert_eq!(preview("one two three four", 9), "one two t…");
        assert_eq!(preview("| a | b |\n|---|---|\n| 1 | 2 |", 50), "a b 1 2");
    }
}
