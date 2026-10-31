//! Review comments on a repository's uncommitted changes and the one prompt they become, shared by
//! every client: the desktop's review panel and the phone's `review` both hand omp exactly this
//! text (through `Command::Review`). Line numbers refer to the file after the change; comments on
//! deleted lines are marked `(removed)`.

/// The prompt must stay below this many bytes.
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// Quoted lines per comment; a longer range gets a trailing `…` line.
pub const MAX_QUOTE_LINES: usize = 12;

/// Which version of the file the line numbers refer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Side {
    /// Added and context lines: the file after the change.
    New,
    /// Deleted lines: the file before the change.
    Old,
}

/// A line range on one side of a diff, with its quoted text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Loc {
    /// Repository-relative path, `/`-separated.
    pub path: String,
    pub span: Span,
}

/// One comment of a review.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Comment {
    /// `None`: a general comment.
    pub loc: Option<Loc>,
    pub body: String,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    #[error("There are no review comments yet")]
    Empty,
    #[error(
        "The review is too large to hand to omp ({} KiB, the limit is {} KiB): shorten or delete some comments",
        .bytes.div_ceil(1024),
        MAX_PROMPT_BYTES / 1024
    )]
    TooLarge { bytes: usize },
}

/// The first paragraph of every review prompt.
pub const HEADER: &str = "Review comments on my uncommitted changes. Paths are relative to the repository root. Line numbers refer to the file after the change; items marked (removed) refer to lines the change deleted.";

/// The review text: a header, then one numbered item per comment in the given order — `path:start[-end]`
/// (or `General`), the quoted lines with `> `, then the comment. Comments with a blank body are
/// skipped; nothing left is [`PromptError::Empty`].
pub fn build_prompt(comments: &[Comment]) -> Result<String, PromptError> {
    let mut out = String::from(HEADER);
    let mut n = 0usize;
    for c in comments {
        let body = c.body.trim();
        if body.is_empty() {
            continue;
        }
        n += 1;
        out.push_str("\n\n");
        out.push_str(&n.to_string());
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
        out.push_str(body);
        if out.len() > MAX_PROMPT_BYTES {
            return Err(PromptError::TooLarge { bytes: out.len() });
        }
    }
    if n == 0 {
        return Err(PromptError::Empty);
    }
    Ok(out)
}

/// The span of lines `start..=end` of a unified diff (`git diff` text): the lines the change
/// added or kept, numbered in the file after the change ([`Side::New`]); when none of them lies in
/// the range but deleted lines do, those, numbered in the file before it ([`Side::Old`]). Lines
/// outside every hunk are not quoted (the span still names them). `start > end` is swapped.
pub fn span_from_diff(diff: &str, start: u32, end: u32) -> Span {
    span_on_side(diff, start, end, None)
}

/// Like [`span_from_diff`], but `Some(side)` quotes exactly that side of the diff (deleted lines
/// for [`Side::Old`], the file after the change for [`Side::New`]) instead of inferring it.
pub fn span_on_side(diff: &str, start: u32, end: u32, side: Option<Side>) -> Span {
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let mut new_lines: Vec<String> = Vec::new();
    let mut new_count = 0usize;
    let mut old_lines: Vec<String> = Vec::new();
    let mut old_count = 0usize;
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            match hunk_starts(rest) {
                Some((o, n)) => {
                    old_no = o;
                    new_no = n;
                    in_hunk = true;
                }
                None => in_hunk = false,
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (marker, text) = line.split_at(line.len().min(1));
        match marker {
            "+" => {
                if (start..=end).contains(&new_no) {
                    new_count += 1;
                    if new_lines.len() < MAX_QUOTE_LINES {
                        new_lines.push(text.to_owned());
                    }
                }
                new_no += 1;
            }
            "-" => {
                if (start..=end).contains(&old_no) {
                    old_count += 1;
                    if old_lines.len() < MAX_QUOTE_LINES {
                        old_lines.push(text.to_owned());
                    }
                }
                old_no += 1;
            }
            " " | "" => {
                if (start..=end).contains(&new_no) {
                    new_count += 1;
                    if new_lines.len() < MAX_QUOTE_LINES {
                        new_lines.push(text.to_owned());
                    }
                }
                old_no += 1;
                new_no += 1;
            }
            // `\ No newline at end of file` and anything else: not a line of the file.
            _ => {}
        }
    }
    let old_wins = match side {
        Some(Side::Old) => true,
        Some(Side::New) => false,
        None => new_count == 0 && old_count > 0,
    };
    if old_wins {
        return Span {
            side: Side::Old,
            start,
            end,
            more: old_count > old_lines.len(),
            quote: old_lines,
        };
    }
    Span {
        side: Side::New,
        start,
        end,
        more: new_count > new_lines.len(),
        quote: new_lines,
    }
}

/// `-a[,b] +c[,d] @@…` → the first old and new line numbers of a hunk.
fn hunk_starts(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let first = |s: &str| s.split(',').next()?.parse::<u32>().ok();
    // A hunk of zero lines on a side starts *after* its number (`-0,0` for a new file).
    Some((first(old)?.max(1), first(new)?.max(1)))
}
