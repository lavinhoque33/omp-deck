//! Review comments of the Changes panel: the pure model and the prompt builder.
//!
//! A comment points at lines of one file (new side for added/context lines, old side for deleted
//! ones) or is general. Comments live in memory per repository root; [`build_prompt`] turns them
//! into the text OmpDeck pastes into omp.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::changes_view::Section;

/// The prompt must stay below this many bytes.
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// Quoted lines per comment; a longer range gets a trailing `…` line.
pub const MAX_QUOTE_LINES: usize = 12;

/// Which version of the file the line numbers refer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Added and context lines: the file after the change.
    New,
    /// Deleted lines: the file before the change.
    Old,
}

/// A line range on one side of a diff, with its quoted text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub side: Side,
    pub start: u32,
    pub end: u32,
    /// The first [`MAX_QUOTE_LINES`] lines of the range, without the diff marker.
    pub quote: Vec<String>,
    /// The range had more lines than `quote` holds.
    pub more: bool,
}

impl Span {
    pub fn contains(&self, side: Side, line: u32) -> bool {
        self.side == side && (self.start..=self.end).contains(&line)
    }

    /// `3`, `3-5`.
    pub fn label(&self) -> String {
        if self.start == self.end {
            self.start.to_string()
        } else {
            format!("{}-{}", self.start, self.end)
        }
    }
}

/// Where a line comment sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    /// Repository-relative path, `/`-separated.
    pub path: String,
    /// The section whose diff the comment was made in (to jump back to it).
    pub section: Section,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    /// `None`: a general comment.
    pub loc: Option<Loc>,
    pub body: String,
}

/// The comments of one repository.
#[derive(Debug, Default)]
pub struct Review {
    comments: Vec<Comment>,
    next_id: u64,
}

impl Review {
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    pub fn len(&self) -> usize {
        self.comments.len()
    }

    /// Adds a comment (the body is trimmed; an empty one is refused) and returns its id.
    pub fn add(&mut self, loc: Option<Loc>, body: &str) -> Option<u64> {
        let body = body.trim();
        if body.is_empty() {
            return None;
        }
        self.next_id += 1;
        self.comments.push(Comment {
            id: self.next_id,
            loc,
            body: body.to_owned(),
        });
        Some(self.next_id)
    }

    /// Replaces the body of comment `id`; false when it is gone or the new body is empty.
    pub fn set_body(&mut self, id: u64, body: &str) -> bool {
        let body = body.trim();
        match self.comments.iter_mut().find(|c| c.id == id) {
            Some(c) if !body.is_empty() => {
                body.clone_into(&mut c.body);
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, id: u64) -> Option<&Comment> {
        self.comments.iter().find(|c| c.id == id)
    }

    pub fn remove(&mut self, id: u64) {
        self.comments.retain(|c| c.id != id);
    }

    /// Drops the comments that were delivered (others added meanwhile stay).
    pub fn remove_all(&mut self, ids: &[u64]) {
        self.comments.retain(|c| !ids.contains(&c.id));
    }
}

/// Reviews of every repository root seen this run (not persisted).
#[derive(Debug, Default)]
pub struct Reviews {
    by_root: Vec<(PathBuf, Review)>,
}

impl Reviews {
    pub fn of(&self, root: &Path) -> Option<&Review> {
        self.by_root.iter().find(|(r, _)| r == root).map(|(_, v)| v)
    }

    /// The review of `root`, created empty on first use (no allocation once it exists).
    pub fn of_mut(&mut self, root: &Path) -> &mut Review {
        let i = match self.by_root.iter().position(|(r, _)| r == root) {
            Some(i) => i,
            None => {
                self.by_root.push((root.to_path_buf(), Review::default()));
                self.by_root.len() - 1
            }
        };
        &mut self.by_root[i].1
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PromptError {
    Empty,
    TooLarge { bytes: usize },
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromptError::Empty => f.write_str("There are no review comments yet"),
            PromptError::TooLarge { bytes } => write!(
                f,
                "The review is too large to hand to omp ({} KiB, the limit is {} KiB): shorten or delete some comments",
                bytes.div_ceil(1024),
                MAX_PROMPT_BYTES / 1024
            ),
        }
    }
}

const HEADER: &str = "Review comments on my uncommitted changes. Paths are relative to the repository root. Line numbers refer to the file after the change; items marked (removed) refer to lines the change deleted.";

/// The review text: a header, then one numbered item per comment in the given order — `path:start[-end]`
/// (or `General`), the quoted lines with `> `, then the comment.
pub fn build_prompt(comments: &[Comment]) -> Result<String, PromptError> {
    if comments.is_empty() {
        return Err(PromptError::Empty);
    }
    let mut out = String::from(HEADER);
    for (n, c) in comments.iter().enumerate() {
        out.push_str("\n\n");
        out.push_str(&(n + 1).to_string());
        out.push_str(". ");
        match &c.loc {
            None => out.push_str("General"),
            Some(l) => {
                out.push_str(&l.path);
                out.push(':');
                out.push_str(&l.span.label());
                if l.span.side == Side::Old {
                    out.push_str(" (removed)");
                }
                for q in l.span.quote.iter().take(MAX_QUOTE_LINES) {
                    out.push_str("\n> ");
                    out.push_str(q);
                }
                if l.span.more {
                    out.push_str("\n> …");
                }
            }
        }
        out.push('\n');
        out.push_str(c.body.trim());
        if out.len() > MAX_PROMPT_BYTES {
            return Err(PromptError::TooLarge { bytes: out.len() });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(side: Side, start: u32, end: u32, quote: &[&str], more: bool) -> Span {
        Span {
            side,
            start,
            end,
            quote: quote.iter().map(|s| (*s).to_owned()).collect(),
            more,
        }
    }

    fn loc(path: &str, span: Span) -> Option<Loc> {
        Some(Loc {
            path: path.to_owned(),
            section: Section::Changes,
            span,
        })
    }

    #[test]
    fn small_review_has_exactly_this_text() {
        let mut r = Review::default();
        r.add(
            loc("src/a.rs", span(Side::New, 3, 3, &["let x = 1;"], false)),
            "  rename x\nto count  ",
        );
        r.add(
            loc(
                "src/a.rs",
                span(Side::Old, 7, 8, &["old one", "old two"], false),
            ),
            "why was this removed?",
        );
        r.add(None, "Please add tests.");
        let text = build_prompt(r.comments()).unwrap();
        assert_eq!(
            text,
            format!(
                "{HEADER}\n\n\
                 1. src/a.rs:3\n> let x = 1;\nrename x\nto count\n\n\
                 2. src/a.rs:7-8 (removed)\n> old one\n> old two\nwhy was this removed?\n\n\
                 3. General\nPlease add tests."
            )
        );
    }

    #[test]
    fn long_ranges_quote_twelve_lines_then_an_ellipsis() {
        let quote: Vec<String> = (0..MAX_QUOTE_LINES).map(|i| format!("l{i}")).collect();
        let refs: Vec<&str> = quote.iter().map(String::as_str).collect();
        let mut r = Review::default();
        r.add(loc("f", span(Side::New, 1, 40, &refs, true)), "too long");
        let text = build_prompt(r.comments()).unwrap();
        assert_eq!(text.matches("\n> l").count(), MAX_QUOTE_LINES);
        assert!(text.contains("\n> l11\n> …\ntoo long"));
        assert!(text.contains("f:1-40\n"));
    }

    #[test]
    fn empty_review_and_empty_bodies_are_refused() {
        assert_eq!(build_prompt(&[]), Err(PromptError::Empty));
        let mut r = Review::default();
        assert_eq!(r.add(None, " \n\t"), None);
        let id = r.add(None, "x").unwrap();
        assert!(!r.set_body(id, "  "));
        assert!(r.set_body(id, " y "));
        assert_eq!(r.get(id).unwrap().body, "y");
    }

    #[test]
    fn a_review_over_the_cap_is_refused_with_a_clear_message() {
        let mut r = Review::default();
        r.add(None, &"x".repeat(MAX_PROMPT_BYTES));
        let err = build_prompt(r.comments()).unwrap_err();
        assert!(matches!(err, PromptError::TooLarge { bytes } if bytes > MAX_PROMPT_BYTES));
        let msg = err.to_string();
        assert!(msg.contains("too large") && msg.contains("32 KiB"), "{msg}");
        // Just below the cap still builds.
        let mut ok = Review::default();
        ok.add(None, &"x".repeat(MAX_PROMPT_BYTES - HEADER.len() - 20));
        assert!(build_prompt(ok.comments()).unwrap().len() <= MAX_PROMPT_BYTES);
    }

    #[test]
    fn span_contains_respects_the_side() {
        let s = span(Side::Old, 4, 6, &[], false);
        assert!(s.contains(Side::Old, 5));
        assert!(!s.contains(Side::New, 5));
        assert!(!s.contains(Side::Old, 7));
        assert_eq!(s.label(), "4-6");
    }

    #[test]
    fn delivered_comments_go_and_later_ones_stay() {
        let mut r = Review::default();
        let a = r.add(None, "a").unwrap();
        let ids = vec![a];
        let b = r.add(None, "b").unwrap();
        r.remove_all(&ids);
        assert_eq!(r.comments().iter().map(|c| c.id).collect::<Vec<_>>(), [b]);
        r.remove(b);
        assert!(r.len() == 0 && r.get(a).is_none());
    }

    #[test]
    fn reviews_are_kept_per_root() {
        let mut all = Reviews::default();
        all.of_mut(Path::new("/a")).add(None, "one");
        assert_eq!(all.of(Path::new("/a")).map(Review::len), Some(1));
        assert!(all.of(Path::new("/b")).is_none());
    }
}
