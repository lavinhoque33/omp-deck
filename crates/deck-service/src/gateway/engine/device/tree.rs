//! The session tree as pages of [`api::TreeNode`] (`docs/notes/mobile-pwa.md`, `tree`): omp's
//! tree filters (the desktop tree view's rules), hidden entries lifted out so a listed node's
//! `parent` is its nearest listed ancestor, file order, an opaque cursor.

use std::ops::Range;

use deck_core::tree::{NodeKind, SessionTree};
use deck_core::usage::parse_iso_ms;
use deck_remote::api::{self, TreeFilter};

use super::cut_chars;

/// One page of a tree listing.
pub(super) struct Page {
    pub nodes: Vec<api::TreeNode>,
    /// The listed node "you are here" stands on.
    pub active_leaf: Option<String>,
    pub start: usize,
    pub total: usize,
    pub window: Window,
}

/// Where a page stays anchored if the wire cap limits how many of its nodes fit.
pub(super) enum Window {
    Forward,
    Backward,
    /// The leaf's position relative to this page's original start.
    Centered(usize),
}

impl Window {
    pub(super) fn clip(&self, total: usize, keep: usize) -> Range<usize> {
        let start = match *self {
            Self::Forward => 0,
            Self::Backward => total - keep,
            Self::Centered(leaf) => centered_start(leaf, total, keep),
        };
        start..start + keep
    }
}

fn centered_start(leaf: usize, total: usize, limit: usize) -> usize {
    if limit == 0 {
        return leaf;
    }
    (leaf + 1)
        .saturating_add(limit / 2)
        .min(total)
        .saturating_sub(limit)
}

/// Omp's `default` filter hides bookkeeping entries and assistant turns without text that
/// ended normally.
fn default_visible(tree: &SessionTree, i: usize) -> bool {
    let n = &tree.nodes[i];
    match &n.kind {
        NodeKind::Label
        | NodeKind::Custom(_)
        | NodeKind::ModelChange
        | NodeKind::ModelUsage
        | NodeKind::ThinkingChange
        | NodeKind::Setting(_) => false,
        NodeKind::Assistant => {
            n.has_text
                || n.stop_reason
                    .as_deref()
                    .is_some_and(|r| r != "stop" && r != "toolUse")
        }
        _ => true,
    }
}

fn visible(tree: &SessionTree, i: usize, filter: TreeFilter, leaf: Option<usize>) -> bool {
    let n = &tree.nodes[i];
    match filter {
        TreeFilter::All => true,
        TreeFilter::Labeled => tree.labels.contains_key(&n.id),
        TreeFilter::Conversation => match n.kind {
            NodeKind::User | NodeKind::Compaction | NodeKind::BranchSummary => true,
            NodeKind::Assistant => n.has_text,
            _ => false,
        },
        TreeFilter::Default => {
            default_visible(tree, i) || (Some(i) == leaf && n.kind == NodeKind::Assistant)
        }
    }
}

fn kind(k: &NodeKind) -> (api::TreeNodeKind, Option<String>) {
    use api::TreeNodeKind as K;
    match k {
        NodeKind::User => (K::User, None),
        NodeKind::Assistant => (K::Assistant, None),
        NodeKind::ToolResult => (K::ToolResult, None),
        NodeKind::Developer => (K::Developer, None),
        NodeKind::BashExecution => (K::BashExecution, None),
        NodeKind::OtherRole(r) => (K::OtherRole, Some(r.clone())),
        NodeKind::CustomMessage(t) => (K::CustomMessage, Some(t.clone())),
        NodeKind::Compaction => (K::Compaction, None),
        NodeKind::BranchSummary => (K::BranchSummary, None),
        NodeKind::ModelChange => (K::ModelChange, None),
        NodeKind::ModelUsage => (K::ModelUsage, None),
        NodeKind::ThinkingChange => (K::ThinkingChange, None),
        NodeKind::Label => (K::Label, None),
        NodeKind::Custom(t) => (K::Custom, Some(t.clone())),
        NodeKind::Setting(t) => (K::Setting, Some(t.clone())),
    }
}

/// A page boundary: `t-` starts at the offset, `b-` ends there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Boundary {
    Start(usize),
    End(usize),
}

impl Boundary {
    pub(super) fn parse(cursor: &str) -> Option<Self> {
        let (direction, digits) = cursor.split_once('-')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let offset = digits.parse().ok()?;
        match direction {
            "t" => Some(Self::Start(offset)),
            "b" => Some(Self::End(offset)),
            _ => None,
        }
    }

    pub(super) fn cursor(self) -> String {
        match self {
            Self::Start(offset) => format!("t-{offset}"),
            Self::End(offset) => format!("b-{offset}"),
        }
    }
}

/// File-order nodes under `filter`. An explicit live empty leaf is distinct from a
/// closed session, which uses the file's leaf.
pub(super) fn page(
    tree: &SessionTree,
    filter: TreeFilter,
    live_leaf: Option<Option<&str>>,
    boundary: Option<Boundary>,
    at_leaf: bool,
    limit: usize,
) -> Page {
    let leaf = match live_leaf {
        Some(Some(id)) => tree.index.get(id).copied().or(tree.file_leaf),
        Some(None) => None,
        None => tree.file_leaf,
    };
    let shown: Vec<bool> = (0..tree.nodes.len())
        .map(|i| visible(tree, i, filter, leaf))
        .collect();
    // The nearest listed ancestor (a parent cycle ends the walk).
    let listed_ancestor = |mut i: usize| -> Option<usize> {
        for _ in 0..tree.nodes.len() {
            i = tree.parent_index(i)?;
            if shown[i] {
                return Some(i);
            }
        }
        None
    };
    let active = leaf.and_then(|l| {
        if shown[l] {
            Some(l)
        } else {
            listed_ancestor(l)
        }
    });
    let active_leaf = active.map(|i| tree.nodes[i].id.clone());
    let listed: Vec<usize> = (0..tree.nodes.len()).filter(|&i| shown[i]).collect();
    let total = listed.len();
    let (start, end, window) = match boundary {
        Some(Boundary::End(end)) => {
            let end = end.min(total);
            (end.saturating_sub(limit), end, Window::Backward)
        }
        Some(Boundary::Start(start)) => {
            let start = start.min(total);
            (
                start,
                start.saturating_add(limit).min(total),
                Window::Forward,
            )
        }
        None => {
            let position = active
                .filter(|_| at_leaf)
                .and_then(|i| listed.binary_search(&i).ok());
            let (start, window) = match position {
                Some(leaf) => {
                    let start = centered_start(leaf, total, limit);
                    (start, Window::Centered(leaf - start))
                }
                None => (0, Window::Forward),
            };
            (start, start.saturating_add(limit).min(total), window)
        }
    };
    let nodes = listed[start..end]
        .iter()
        .map(|&i| {
            let n = &tree.nodes[i];
            let (kind, subtype) = kind(&n.kind);
            api::TreeNode {
                id: n.id.clone(),
                parent: listed_ancestor(i).map(|p| tree.nodes[p].id.clone()),
                kind,
                subtype,
                at_unix_ms: n
                    .timestamp
                    .as_deref()
                    .and_then(parse_iso_ms)
                    .and_then(|ms| u64::try_from(ms).ok()),
                text: cut_chars(&n.text, api::TREE_TEXT_CHARS),
                has_text: n.has_text,
                stop_reason: n.stop_reason.clone(),
                label: tree
                    .labels
                    .get(&n.id)
                    .map(|label| cut_chars(label, api::TREE_TEXT_CHARS)),
                forkable: tree.is_forkable(i),
                cost_before: Some(n.cost_before.total).filter(|c| c.is_finite() && *c > 0.0),
                cost_incomplete: n.cost_before.incomplete,
            }
        })
        .collect();
    Page {
        nodes,
        active_leaf,
        start,
        total,
        window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(lines: &[serde_json::Value]) -> SessionTree {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();
        SessionTree::load(&path).unwrap()
    }

    fn sample() -> SessionTree {
        use serde_json::json;
        tree(&[
            json!({"type":"session","id":"s","cwd":"/p","timestamp":"2026-10-29T00:00:00Z"}),
            json!({"type":"message","id":"u1","parentId":null,"timestamp":"2026-10-29T00:00:01Z",
                "message":{"role":"user","content":"hello"}}),
            json!({"type":"model_change","id":"m1","parentId":"u1","timestamp":"2026-10-29T00:00:02Z",
                "provider":"p","modelId":"m"}),
            json!({"type":"message","id":"a1","parentId":"m1","timestamp":"2026-10-29T00:00:03Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"stopReason":"stop"}}),
            json!({"type":"message","id":"u2","parentId":"a1","timestamp":"2026-10-29T00:00:04Z",
                "message":{"role":"user","content":"again"}}),
        ])
    }

    #[test]
    fn tree_labels_have_the_same_unicode_character_limit_as_node_text() {
        let mut t = sample();
        let at_limit = "🧪".repeat(api::TREE_TEXT_CHARS);
        t.labels.insert("u1".to_owned(), format!("{at_limit}é"));
        let p = page(&t, TreeFilter::Conversation, None, None, false, 2);
        assert_eq!(p.nodes[0].label.as_deref(), Some(at_limit.as_str()));
        assert_eq!(p.nodes[1].label, None);
    }

    #[test]
    fn hidden_entries_are_lifted_and_pages_continue() {
        let t = sample();
        let all = page(&t, TreeFilter::All, None, None, false, 100);
        assert!(all.nodes.iter().any(|n| n.id == "m1"));
        let conv = page(&t, TreeFilter::Conversation, None, None, false, 100);
        let ids: Vec<&str> = conv.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["u1", "a1", "u2"]);
        // `a1`'s parent `m1` is hidden: it hangs below `u1`.
        assert_eq!(conv.nodes[1].parent.as_deref(), Some("u1"));
        assert_eq!(conv.active_leaf.as_deref(), Some("u2"));
        assert_eq!(conv.total, 3);
        let first = page(
            &t,
            TreeFilter::Conversation,
            Some(Some("a1")),
            None,
            false,
            2,
        );
        assert_eq!((first.start, first.nodes.len(), first.total), (0, 2, 3));
        assert_eq!(first.active_leaf.as_deref(), Some("a1"));
        let rest = page(
            &t,
            TreeFilter::Conversation,
            None,
            Some(Boundary::Start(2)),
            true,
            2,
        );
        assert_eq!(rest.start, 2);
        assert_eq!(rest.nodes[0].id, "u2");
        // The user prompt after a reply forks before itself; the first one cannot.
        assert!(!conv.nodes[0].forkable);
        assert!(conv.nodes[2].forkable);
    }

    #[test]
    fn cursors_are_opaque_offsets() {
        for cursor in [Boundary::Start(42), Boundary::End(42)] {
            assert_eq!(Boundary::parse(&cursor.cursor()), Some(cursor));
        }
        for bad in ["42", "t-", "t-+1", "t-x", "b-", "b--1", "f-1", "/home"] {
            assert_eq!(Boundary::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn centered_and_backward_pages_use_listed_positions() {
        let t = sample();
        let p = page(&t, TreeFilter::All, Some(Some("a1")), None, true, 3);
        assert_eq!((p.start, p.total), (1, 4));
        assert_eq!(p.active_leaf.as_deref(), Some("a1"));
        assert_eq!(
            p.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["m1", "a1", "u2"]
        );
        let previous = page(
            &t,
            TreeFilter::All,
            None,
            Some(Boundary::End(p.start)),
            true,
            3,
        );
        assert_eq!((previous.start, previous.total), (0, 4));
        assert_eq!(previous.nodes.len(), 1);
        assert_eq!(previous.nodes[0].id, "u1");

        let at_start = page(
            &t,
            TreeFilter::Conversation,
            Some(Some("m1")),
            None,
            true,
            2,
        );
        assert_eq!(at_start.start, 0);
        assert_eq!(at_start.active_leaf.as_deref(), Some("u1"));
        let no_leaf = page(&t, TreeFilter::All, Some(None), None, true, 2);
        assert_eq!(no_leaf.start, 0);
        assert_eq!(no_leaf.active_leaf, None);
        let clamped = page(
            &t,
            TreeFilter::All,
            None,
            Some(Boundary::End(usize::MAX)),
            false,
            2,
        );
        assert_eq!((clamped.start, clamped.nodes.len()), (2, 2));
    }
}
