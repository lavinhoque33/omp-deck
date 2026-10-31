//! The session tree as pages of [`api::TreeNode`] (`docs/notes/mobile-pwa.md`, `tree`): omp's
//! tree filters (the desktop tree view's rules), hidden entries lifted out so a listed node's
//! `parent` is its nearest listed ancestor, file order, an opaque cursor.

use deck_core::tree::{NodeKind, SessionTree};
use deck_core::usage::parse_iso_ms;
use deck_remote::api::{self, TreeFilter};

use super::cut_chars;

/// One page of a tree listing.
pub(super) struct Page {
    pub nodes: Vec<api::TreeNode>,
    /// The listed node "you are here" stands on.
    pub active_leaf: Option<String>,
    /// Where the listing continues (`None`: this was the last page).
    pub next: Option<usize>,
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

/// The page cursor of `offset`.
pub(super) fn cursor(offset: usize) -> String {
    format!("t-{offset}")
}

/// The offset of a cursor this gateway handed out.
pub(super) fn offset(cursor: &str) -> Option<usize> {
    let digits = cursor.strip_prefix("t-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The nodes `filter` lists, from `start`, at most `limit`. `live_leaf`: the live session's
/// current entry (else the file's last one).
pub(super) fn page(
    tree: &SessionTree,
    filter: TreeFilter,
    live_leaf: Option<&str>,
    start: usize,
    limit: usize,
) -> Page {
    let leaf = live_leaf
        .and_then(|id| tree.index.get(id).copied())
        .or(tree.file_leaf);
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
    let active_leaf = leaf
        .and_then(|l| {
            if shown[l] {
                Some(l)
            } else {
                listed_ancestor(l)
            }
        })
        .map(|i| tree.nodes[i].id.clone());
    let listed: Vec<usize> = (0..tree.nodes.len()).filter(|&i| shown[i]).collect();
    let start = start.min(listed.len());
    let end = start.saturating_add(limit).min(listed.len());
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
                label: tree.labels.get(&n.id).cloned(),
                forkable: tree.is_forkable(i),
                cost_before: Some(n.cost_before.total).filter(|c| c.is_finite() && *c > 0.0),
                cost_incomplete: n.cost_before.incomplete,
            }
        })
        .collect();
    Page {
        nodes,
        active_leaf,
        next: (end < listed.len()).then_some(end),
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
    fn hidden_entries_are_lifted_and_pages_continue() {
        let t = sample();
        let all = page(&t, TreeFilter::All, None, 0, 100);
        assert!(all.nodes.iter().any(|n| n.id == "m1"));
        let conv = page(&t, TreeFilter::Conversation, None, 0, 100);
        let ids: Vec<&str> = conv.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["u1", "a1", "u2"]);
        // `a1`'s parent `m1` is hidden: it hangs below `u1`.
        assert_eq!(conv.nodes[1].parent.as_deref(), Some("u1"));
        assert_eq!(conv.active_leaf.as_deref(), Some("u2"));
        assert!(conv.next.is_none());
        let first = page(&t, TreeFilter::Conversation, Some("a1"), 0, 2);
        assert_eq!(first.next, Some(2));
        assert_eq!(first.active_leaf.as_deref(), Some("a1"));
        let rest = page(&t, TreeFilter::Conversation, None, 2, 2);
        assert_eq!(rest.nodes.len(), 1);
        assert!(rest.next.is_none());
        // The user prompt after a reply forks before itself; the first one cannot.
        assert!(!conv.nodes[0].forkable);
        assert!(conv.nodes[2].forkable);
    }

    #[test]
    fn cursors_are_opaque_offsets() {
        assert_eq!(offset(&cursor(42)), Some(42));
        for bad in ["42", "t-", "t-+1", "t-x", "/home"] {
            assert_eq!(offset(bad), None, "{bad}");
        }
    }
}
