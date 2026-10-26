//! Pure layout of the session tree view: which nodes are shown for a filter, how hidden nodes
//! lift their visible descendants, which lane each row draws in (git-graph style) and the row
//! model the view paints. No egui here; the view caches one [`Layout`] per (tree revision,
//! leaf, filter).

use std::ops::Range;

use deck_core::tree::{NodeKind, SessionTree, TreeNode};

/// Which nodes are listed. Mirrors omp's `/tree` filters, plus the GUI's conversation view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Filter {
    /// User prompts, assistant replies with text, compactions and branch summaries.
    #[default]
    Conversation,
    /// omp's `/tree` default: everything except settings-like entries and empty assistant steps.
    Default,
    All,
    /// Only nodes that carry a label.
    Labeled,
}

impl Filter {
    pub const ALL: [Filter; 4] = [
        Filter::Conversation,
        Filter::Default,
        Filter::All,
        Filter::Labeled,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Filter::Conversation => "Conversation",
            Filter::Default => "Default",
            Filter::All => "All",
            Filter::Labeled => "Labeled",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Filter::Conversation => "Your prompts and the assistant's replies, tool calls hidden",
            Filter::Default => "What omp's /tree shows by default",
            Filter::All => "Every entry in the session file",
            Filter::Labeled => "Only entries with a label",
        }
    }
}

/// Visual class of a row (glyph and accent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowClass {
    User,
    Assistant,
    Tool,
    Compaction,
    Summary,
    Other,
}

impl RowClass {
    pub fn of(kind: &NodeKind) -> RowClass {
        match kind {
            NodeKind::User => RowClass::User,
            NodeKind::Assistant => RowClass::Assistant,
            NodeKind::ToolResult | NodeKind::BashExecution => RowClass::Tool,
            NodeKind::Compaction => RowClass::Compaction,
            NodeKind::BranchSummary => RowClass::Summary,
            _ => RowClass::Other,
        }
    }
}

/// One visible node.
#[derive(Clone, Debug)]
pub struct Row {
    /// Index into `SessionTree::nodes`.
    pub node: usize,
    pub lane: u16,
    /// On the root → current-leaf path.
    pub on_path: bool,
    /// The session's current leaf ("you are here").
    pub is_leaf: bool,
    /// Visible (lifted) children; more than one makes this a branch point.
    pub children: u16,
    /// Row of the nearest visible ancestor.
    pub parent: Option<u32>,
    /// Epoch seconds of the node's timestamp.
    pub ts: Option<i64>,
    pub class: RowClass,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Entries in the session file.
    pub entries: usize,
    /// Rows shown under the filter.
    pub shown: usize,
    /// Visible nodes with no visible child: the tips of the branches.
    pub tips: usize,
    /// Visible nodes with more than one visible child.
    pub forks: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Number of lanes used (at least 1 when there are rows).
    pub lanes: u16,
    /// Rows whose edge to the parent spans more than the adjacent row (drawn when their span
    /// crosses the viewport, even if neither end is on screen).
    pub long_edges: Vec<u32>,
    pub leaf_row: Option<usize>,
    /// The session's actual leaf node, listed or not.
    pub leaf_node: Option<usize>,
    pub stats: Stats,
    /// `node index → row` (`u32::MAX` = not shown).
    row_of: Vec<u32>,
}

impl Layout {
    pub fn row_of_node(&self, node: usize) -> Option<usize> {
        self.row_of
            .get(node)
            .copied()
            .filter(|&r| r != u32::MAX)
            .map(|r| r as usize)
    }
}

/// Whether node `i` is listed for `filter`. A leaf that the filter hides is not forced in: "you
/// are here" moves to its nearest listed ancestor (except omp's own rule that an empty
/// assistant step is shown while it is the leaf).
pub fn is_visible(tree: &SessionTree, i: usize, filter: Filter, leaf: Option<usize>) -> bool {
    let n = &tree.nodes[i];
    match filter {
        Filter::All => true,
        Filter::Labeled => tree.labels.contains_key(&n.id),
        Filter::Default => {
            default_visible(n) || (Some(i) == leaf && matches!(n.kind, NodeKind::Assistant))
        }
        Filter::Conversation => match n.kind {
            NodeKind::User | NodeKind::Compaction | NodeKind::BranchSummary => true,
            NodeKind::Assistant => n.has_text,
            _ => false,
        },
    }
}

/// omp's `/tree` default view (docs/notes/session-tree.md §1 "Visibility").
fn default_visible(n: &TreeNode) -> bool {
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

/// Lay the tree out for `filter`. Hidden nodes drop out and their visible descendants attach to
/// the nearest visible ancestor (omp's lift rule). Siblings order: the child on the path to
/// `leaf` first, then by time. Lane 0 carries the active path; every other sibling opens a new
/// lane (the lowest one free for its whole span) that closes after its subtree.
pub fn build(tree: &SessionTree, leaf: Option<usize>, filter: Filter) -> Layout {
    let n = tree.nodes.len();
    let leaf = leaf.filter(|&l| l < n);
    let on_path = {
        let mut p = tree.active_path(leaf);
        p.resize(n, false);
        p
    };

    // Lift: visible children per nearest visible ancestor, in file/time order.
    let mut vkids: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut vroots: Vec<u32> = Vec::new();
    let mut stack: Vec<(usize, Option<usize>)> =
        tree.roots.iter().rev().map(|&r| (r, None)).collect();
    while let Some((i, anc)) = stack.pop() {
        let mut next_anc = anc;
        if is_visible(tree, i, filter, leaf) {
            match anc {
                Some(a) => vkids[a].push(i as u32),
                None => vroots.push(i as u32),
            }
            next_anc = Some(i);
        }
        if let Some(kids) = tree.children.get(i) {
            stack.extend(kids.iter().rev().map(|&c| (c, next_anc)));
        }
    }
    // The branch holding the leaf leads (stable: time order is kept within each group).
    let lead = |c: &u32| !on_path[*c as usize];
    vroots.sort_by_key(lead);
    for k in vkids.iter_mut().filter(|k| k.len() > 1) {
        k.sort_by_key(lead);
    }

    // Pre-order rows: (node, parent row, index among the parent's visible children).
    let mut order: Vec<(u32, Option<u32>, u32)> = Vec::new();
    let mut dfs: Vec<(u32, Option<u32>, u32)> = vroots
        .iter()
        .enumerate()
        .rev()
        .map(|(i, &r)| (r, None, i as u32))
        .collect();
    while let Some((node, parent, idx)) = dfs.pop() {
        let row = order.len() as u32;
        order.push((node, parent, idx));
        dfs.extend(
            vkids[node as usize]
                .iter()
                .enumerate()
                .rev()
                .map(|(i, &c)| (c, Some(row), i as u32)),
        );
    }
    let count = order.len();
    let mut size = vec![1u32; count];
    for r in (0..count).rev() {
        if let Some(p) = order[r].1 {
            size[p as usize] += size[r];
        }
    }

    // Lanes: busy row intervals per lane.
    let mut busy: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut lane_of = vec![0u16; count];
    for r in 0..count {
        let (_, parent, idx) = order[r];
        let end = r as u32 + size[r] - 1;
        let lane = match parent {
            Some(p) if idx == 0 => lane_of[p as usize],
            _ => {
                let (from, start) = match parent {
                    Some(p) => (lane_of[p as usize] as usize + 1, p + 1),
                    None => (0, r as u32),
                };
                let mut l = from;
                while busy
                    .get(l)
                    .is_some_and(|b| b.iter().any(|&(a, z)| a <= end && z >= start))
                {
                    l += 1;
                }
                if l == busy.len() {
                    busy.push(Vec::new());
                }
                busy[l].push((start, end));
                l as u16
            }
        };
        lane_of[r] = lane;
    }

    let mut row_of = vec![u32::MAX; n];
    let mut rows = Vec::with_capacity(count);
    let mut long_edges = Vec::new();
    let mut stats = Stats {
        entries: n,
        shown: count,
        ..Stats::default()
    };
    for (r, &(node, parent, _)) in order.iter().enumerate() {
        let i = node as usize;
        let kids = vkids[i].len();
        match kids {
            0 => stats.tips += 1,
            1 => {}
            _ => stats.forks += 1,
        }
        if parent.is_some_and(|p| p as usize + 1 != r) {
            long_edges.push(r as u32);
        }
        row_of[i] = r as u32;
        rows.push(Row {
            node: i,
            lane: lane_of[r],
            on_path: on_path[i],
            is_leaf: false,
            children: kids.min(u16::MAX as usize) as u16,
            parent,
            ts: tree.nodes[i].timestamp.as_deref().and_then(parse_iso),
            class: RowClass::of(&tree.nodes[i].kind),
        });
    }
    // "You are here": the leaf's row, or its nearest listed ancestor when the filter hides it.
    let mut here = leaf;
    while let Some(i) = here
        && row_of[i] == u32::MAX
    {
        here = tree.parent_index(i);
    }
    let leaf_row = here.map(|i| row_of[i] as usize);
    if let Some(r) = leaf_row {
        rows[r].is_leaf = true;
    }
    Layout {
        lanes: busy.len().max(usize::from(count > 0)) as u16,
        rows,
        long_edges,
        leaf_row,
        leaf_node: leaf,
        stats,
        row_of,
    }
}

// ------------------------------------------------------------------ text, search, time

/// Lowercased whitespace-separated search words.
pub fn search_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

/// What the search matches a node against: kind name, label and row text.
fn haystack(tree: &SessionTree, node: usize) -> String {
    let n = &tree.nodes[node];
    let mut s = String::with_capacity(n.text.len() + 24);
    s.push_str(kind_name(&n.kind));
    s.push(' ');
    if let Some(l) = tree.labels.get(&n.id) {
        s.push_str(l);
        s.push(' ');
    }
    s.push_str(&n.text);
    s.make_ascii_lowercase();
    s
}

/// Per row: every search word occurs in the node's kind, label or text (ASCII case-insensitive).
pub fn search_matches(tree: &SessionTree, layout: &Layout, tokens: &[String]) -> Vec<bool> {
    layout
        .rows
        .iter()
        .map(|r| {
            let h = haystack(tree, r.node);
            tokens.iter().all(|t| h.contains(t.as_str()))
        })
        .collect()
}

/// Byte ranges of `text` the search words occur in, merged and sorted.
pub fn highlight_spans(text: &str, tokens: &[String]) -> Vec<Range<usize>> {
    let lower = text.to_ascii_lowercase();
    let mut spans: Vec<Range<usize>> = Vec::new();
    for t in tokens.iter().filter(|t| !t.is_empty()) {
        let mut from = 0;
        while let Some(at) = lower[from..].find(t.as_str()) {
            let start = from + at;
            spans.push(start..start + t.len());
            from = start + t.len();
        }
    }
    spans.sort_by_key(|s| s.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for s in spans {
        match merged.last_mut() {
            Some(last) if s.start <= last.end => last.end = last.end.max(s.end),
            _ => merged.push(s),
        }
    }
    merged
}

pub fn kind_name(kind: &NodeKind) -> &str {
    match kind {
        NodeKind::User => "user",
        NodeKind::Assistant => "assistant",
        NodeKind::ToolResult => "tool",
        NodeKind::Developer => "developer",
        NodeKind::BashExecution => "bash",
        NodeKind::OtherRole(r) => r,
        NodeKind::CustomMessage(c) => c,
        NodeKind::Compaction => "compaction",
        NodeKind::BranchSummary => "summary",
        NodeKind::ModelChange => "model",
        NodeKind::ModelUsage => "model usage",
        NodeKind::ThinkingChange => "thinking",
        NodeKind::Label => "label",
        NodeKind::Custom(c) => c,
        NodeKind::Setting(s) => s,
    }
}

/// `2026-10-21T12:34:56.789Z` (any offset is ignored beyond `Z`/none) → epoch seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0i64, |a, d| a * 10 + i64::from(d - b'0')))
    };
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || !matches!(b.get(10), Some(b'T' | b' '))
    {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// Compact relative age for the right-hand column: `now`, `5m`, `3h`, `2d`, `6w`, `8mo`, `2y`.
pub fn age(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=44 => "now".to_owned(),
        45..=3_599 => format!("{}m", (d + 30) / 60),
        3_600..=86_399 => format!("{}h", d / 3_600),
        86_400..=1_209_599 => format!("{}d", d / 86_400),
        1_209_600..=5_183_999 => format!("{}w", d / 604_800),
        5_184_000..=31_535_999 => format!("{}mo", d / 2_592_000),
        _ => format!("{}y", d / 31_536_000),
    }
}

/// `2026-10-21 12:34:56 UTC` for tooltips.
pub fn clock(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// Tooltip of the fork button for a node.
pub fn fork_hint(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::User => "Fork before this prompt",
        NodeKind::Assistant => "Fork after this reply",
        _ => "Fork after this message",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    /// (id, parent, kind, has_text)
    type Spec<'a> = (&'a str, Option<&'a str>, NodeKind, bool);

    fn user<'a>(id: &'a str, parent: Option<&'a str>) -> Spec<'a> {
        (id, parent, NodeKind::User, true)
    }
    fn asst<'a>(id: &'a str, parent: Option<&'a str>) -> Spec<'a> {
        (id, parent, NodeKind::Assistant, true)
    }
    fn tool<'a>(id: &'a str, parent: Option<&'a str>) -> Spec<'a> {
        (id, parent, NodeKind::ToolResult, false)
    }

    fn tree(spec: &[Spec<'_>]) -> SessionTree {
        let nodes: Vec<Arc<TreeNode>> = spec
            .iter()
            .enumerate()
            .map(|(i, (id, parent, kind, has_text))| {
                Arc::new(TreeNode {
                    id: (*id).to_owned(),
                    parent: parent.map(str::to_owned),
                    kind: kind.clone(),
                    timestamp: Some(format!("2026-01-01T00:{:02}:{:02}Z", i / 60, i % 60)),
                    text: format!("{id} text"),
                    has_text: *has_text,
                    stop_reason: None,
                    active_leaf: None,
                    cost_before: Default::default(),
                })
            })
            .collect();
        let index: HashMap<Arc<str>, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (Arc::from(n.id.as_str()), i))
            .collect();
        let mut children = vec![Vec::new(); nodes.len()];
        let mut roots = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            match n.parent.as_deref().and_then(|p| index.get(p)) {
                Some(&p) if p != i => children[p].push(i),
                _ => roots.push(i),
            }
        }
        SessionTree {
            nodes,
            index,
            children: children.into_iter().map(Arc::new).collect(),
            roots,
            labels: HashMap::new(),
            file_leaf: None,
        }
    }

    fn ids<'a>(t: &'a SessionTree, l: &Layout) -> Vec<&'a str> {
        l.rows.iter().map(|r| t.nodes[r.node].id.as_str()).collect()
    }
    fn lane_of(t: &SessionTree, l: &Layout, id: &str) -> u16 {
        l.rows[l.row_of_node(t.index[id]).unwrap()].lane
    }

    #[test]
    fn linear_chain_is_one_lane_in_file_order() {
        let t = tree(&[user("a", None), asst("b", Some("a")), user("c", Some("b"))]);
        let l = build(&t, Some(2), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "c"]);
        assert_eq!(l.lanes, 1);
        assert!(l.long_edges.is_empty());
        assert!(l.rows.iter().all(|r| r.lane == 0 && r.on_path));
        assert_eq!(l.leaf_row, Some(2));
        assert_eq!(l.stats.forks, 0);
        assert_eq!(l.stats.tips, 1);
    }

    #[test]
    fn hidden_nodes_lift_their_visible_descendants() {
        // a → t1(tool) → t2(tool) → b ; Conversation hides the tools, b attaches to a.
        let t = tree(&[
            user("a", None),
            tool("t1", Some("a")),
            tool("t2", Some("t1")),
            asst("b", Some("t2")),
        ]);
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b"]);
        assert_eq!(l.rows[1].parent, Some(0));
        let all = build(&t, Some(3), Filter::All);
        assert_eq!(ids(&t, &all), ["a", "t1", "t2", "b"]);
    }

    #[test]
    fn lifting_through_a_hidden_fork_creates_branches_on_the_visible_ancestor() {
        // a → tool → {b, c}: tool is hidden but forks; b and c both lift to a.
        let t = tree(&[
            user("a", None),
            tool("t", Some("a")),
            asst("b", Some("t")),
            asst("c", Some("t")),
        ]);
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(l.rows[0].children, 2);
        assert_eq!(l.stats.forks, 1);
        // The leaf's branch (c) leads and keeps the lane; b branches off.
        assert_eq!(ids(&t, &l), ["a", "c", "b"]);
        assert_eq!(lane_of(&t, &l, "c"), 0);
        assert_eq!(lane_of(&t, &l, "b"), 1);
    }

    #[test]
    fn nested_branches_get_separate_lanes_and_reuse_closed_ones() {
        //        a
        //        └ b ─┬ c ─┬ d        (leaf path: a b c d)
        //             │    └ e
        //             └ f ── g
        //   a2 (second root, own graph)
        let t = tree(&[
            user("a", None),
            asst("b", Some("a")),
            user("c", Some("b")),
            asst("d", Some("c")),
            asst("e", Some("c")),
            user("f", Some("b")),
            asst("g", Some("f")),
            user("a2", None),
        ]);
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "c", "d", "e", "f", "g", "a2"]);
        for id in ["a", "b", "c", "d"] {
            assert_eq!(lane_of(&t, &l, id), 0, "{id}");
        }
        // DFS order: e (processed first) takes lane 1 for rows 3..=4; f's line, which spans
        // rows 2..=6, then has to route around it.
        assert_eq!(lane_of(&t, &l, "e"), 1);
        assert_eq!(lane_of(&t, &l, "f"), 2);
        assert_eq!(lane_of(&t, &l, "g"), 2);
        assert_eq!(l.lanes, 3);
        // The second root starts a fresh graph in lane 0.
        assert_eq!(lane_of(&t, &l, "a2"), 0);
        assert_eq!(l.rows[7].parent, None);
        // Edges that skip rows or leave the parent's lane are listed as long.
        let e = l.row_of_node(t.index["e"]).unwrap() as u32;
        let f = l.row_of_node(t.index["f"]).unwrap() as u32;
        assert!(l.long_edges.contains(&e) && l.long_edges.contains(&f));
        assert_eq!(l.stats.forks, 2);
        assert_eq!(l.stats.tips, 4);
    }

    #[test]
    fn later_branches_route_around_lanes_claimed_by_nested_forks() {
        let t = tree(&[
            user("a", None),
            asst("b", Some("a")),
            asst("c", Some("a")),
            user("d", Some("b")),
            asst("e", Some("d")),
            asst("f", Some("d")),
        ]);
        // Leaf e: path a b d e. Rows: a b d e f c.
        let l = build(&t, Some(4), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "d", "e", "f", "c"]);
        // f (rows 4..=4) is closed before c opens at row 1..=5? c spans rows 1..=5 and f 3..=4,
        // so c routes around f.
        assert_eq!(lane_of(&t, &l, "f"), 1);
        assert_eq!(lane_of(&t, &l, "c"), 2);
        // Without the inner fork the outer branch takes lane 1.
        let t2 = tree(&[user("a", None), asst("b", Some("a")), asst("c", Some("a"))]);
        let l2 = build(&t2, Some(1), Filter::Conversation);
        assert_eq!(lane_of(&t2, &l2, "c"), 1);
    }

    #[test]
    fn active_path_is_marked_and_leads_every_fork() {
        let t = tree(&[
            user("a", None),
            asst("b", Some("a")),
            user("c", Some("b")),
            user("d", Some("b")),
        ]);
        // Leaf = d although c comes first in file order.
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "d", "c"]);
        let on: Vec<bool> = l.rows.iter().map(|r| r.on_path).collect();
        assert_eq!(on, [true, true, true, false]);
        assert!(l.rows[2].is_leaf && !l.rows[3].is_leaf);
        assert_eq!(l.rows[1].children, 2);
        // No leaf: nothing is on the path and file order is kept.
        let none = build(&t, None, Filter::Conversation);
        assert_eq!(ids(&t, &none), ["a", "b", "c", "d"]);
        assert!(none.rows.iter().all(|r| !r.on_path) && none.leaf_row.is_none());
    }

    #[test]
    fn filters_select_the_documented_kinds() {
        let spec = vec![
            user("u", None),
            ("m", Some("u"), NodeKind::ModelChange, false),
            tool("t", Some("m")),
            (
                "e",
                Some("t"),
                NodeKind::Assistant,
                false, // no text: hidden by default/conversation
            ),
            asst("a", Some("e")),
            ("k", Some("a"), NodeKind::Compaction, false),
            ("s", Some("k"), NodeKind::BranchSummary, false),
            ("x", Some("s"), NodeKind::Label, false),
            user("z", Some("x")),
        ];
        let mut t = tree(&spec);
        t.labels.insert("a".to_owned(), "keep".to_owned());
        // Leaf = z so no node is forced visible by being the leaf.
        let leaf = Some(t.index["z"]);
        let conv = build(&t, leaf, Filter::Conversation);
        assert_eq!(ids(&t, &conv), ["u", "a", "k", "s", "z"]);
        let def = build(&t, leaf, Filter::Default);
        assert_eq!(ids(&t, &def), ["u", "t", "a", "k", "s", "z"]);
        let all = build(&t, leaf, Filter::All);
        assert_eq!(all.rows.len(), 9);
        // Labeled lists only the labeled node; the hidden leaf's marker moves up to it.
        let lab = build(&t, leaf, Filter::Labeled);
        assert_eq!(ids(&t, &lab), ["a"]);
        assert!(lab.rows[0].is_leaf);
        // An assistant step that stopped abnormally shows in the default view.
        let mut t2 = tree(&[user("u", None), asst("a", Some("u")), user("v", Some("a"))]);
        Arc::make_mut(&mut t2.nodes[1]).has_text = false;
        Arc::make_mut(&mut t2.nodes[1]).stop_reason = Some("error".to_owned());
        let def2 = build(&t2, Some(2), Filter::Default);
        assert_eq!(ids(&t2, &def2), ["u", "a", "v"]);
        let conv2 = build(&t2, Some(2), Filter::Conversation);
        assert_eq!(ids(&t2, &conv2), ["u", "v"]);
    }

    #[test]
    fn a_hidden_leaf_moves_you_are_here_to_its_nearest_listed_ancestor() {
        let t = tree(&[user("a", None), asst("b", Some("a")), tool("t", Some("b"))]);
        let l = build(&t, Some(2), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b"]);
        assert!(l.rows[1].is_leaf && !l.rows[0].is_leaf);
        assert_eq!(l.leaf_row, Some(1));
        // The path still runs through the hidden node.
        assert!(l.rows.iter().all(|r| r.on_path));
        let all = build(&t, Some(2), Filter::All);
        assert!(all.rows[2].is_leaf);
    }

    #[test]
    fn deep_chains_do_not_recurse() {
        let mut spec: Vec<(String, Option<String>)> = vec![("n0".into(), None)];
        for i in 1..30_000 {
            spec.push((format!("n{i}"), Some(format!("n{}", i - 1))));
        }
        let owned: Vec<Spec<'_>> = spec
            .iter()
            .map(|(id, p)| (id.as_str(), p.as_deref(), NodeKind::User, true))
            .collect();
        let t = tree(&owned);
        let l = build(&t, Some(29_999), Filter::Conversation);
        assert_eq!(l.rows.len(), 30_000);
        assert_eq!(l.lanes, 1);
    }

    #[test]
    fn search_matches_words_anywhere_and_highlights_spans() {
        let mut t = tree(&[user("a", None), asst("b", Some("a"))]);
        Arc::make_mut(&mut t.nodes[0]).text = "Fix the Login bug".to_owned();
        Arc::make_mut(&mut t.nodes[1]).text = "Done".to_owned();
        t.labels.insert("b".to_owned(), "milestone".to_owned());
        let l = build(&t, Some(1), Filter::Conversation);
        let tok = search_tokens("  login FIX ");
        assert_eq!(search_matches(&t, &l, &tok), [true, false]);
        assert_eq!(
            search_matches(&t, &l, &search_tokens("mile")),
            [false, true]
        );
        assert_eq!(
            search_matches(&t, &l, &search_tokens("assistant")),
            [false, true]
        );
        assert_eq!(search_matches(&t, &l, &[]), [true, true]);
        assert_eq!(
            highlight_spans("Fix the Login bug fix", &tok),
            vec![0..3, 8..13, 18..21]
        );
        assert_eq!(highlight_spans("aaaa", &search_tokens("aa a")), vec![0..4]);
    }

    #[test]
    fn iso_time_roundtrip_and_ages() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-21T12:34:56.789Z"), Some(1_792_586_096));
        assert_eq!(clock(1_792_586_096), "2026-10-21 12:34:56 UTC");
        assert_eq!(parse_iso("nonsense"), None);
        assert_eq!(parse_iso("2026-13-01T00:00:00Z"), None);
        assert_eq!(age(100, 90), "now");
        assert_eq!(age(1_000, 700), "5m");
        assert_eq!(age(10_000, 0), "2h");
        assert_eq!(age(3 * 86_400, 0), "3d");
        assert_eq!(age(21 * 86_400, 0), "3w");
        assert_eq!(age(100 * 86_400, 0), "3mo");
        assert_eq!(age(800 * 86_400, 0), "2y");
        assert_eq!(age(0, 50), "now");
    }

    #[test]
    fn fork_hint_names_the_cut() {
        assert_eq!(fork_hint(&NodeKind::User), "Fork before this prompt");
        assert_eq!(fork_hint(&NodeKind::Assistant), "Fork after this reply");
    }
}
