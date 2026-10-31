//! Review comments of the Changes panel: the GUI-side model. A comment points at lines of one
//! file (new side for added/context lines, old side for deleted ones) or is general. Comments
//! live in memory per repository root; handing them to omp is the gateway's `Mutation::Review`,
//! which builds the prompt (`deck_service::review`).

use std::path::{Path, PathBuf};

use crate::changes_view::Section;

pub use deck_service::review::{MAX_QUOTE_LINES, Side, Span};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bodies_are_refused() {
        let mut r = Review::default();
        assert_eq!(r.add(None, " \n\t"), None);
        let id = r.add(None, "x").unwrap();
        assert!(!r.set_body(id, "  "));
        assert!(r.set_body(id, " y "));
        assert_eq!(r.get(id).unwrap().body, "y");
    }

    #[test]
    fn span_contains_respects_the_side() {
        let s = Span {
            side: Side::Old,
            start: 4,
            end: 6,
            quote: Vec::new(),
            more: false,
        };
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
