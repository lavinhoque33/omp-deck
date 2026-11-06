//! The session tree window, mockup B ("Turn trails"): branch-aware turn-trail cards in a
//! left navigator with a graph gutter, and the selected entry's content in a right preview pane.
//! Rows are virtualized with per-class heights; the layout is rebuilt only when the tree revision,
//! the leaf or the filter changes.

mod geometry;
mod keys;
mod paint;
mod preview;

use std::collections::HashMap;
use std::sync::Arc;

use deck_core::config::UiFont;
use deck_core::ctl::BranchSummary;
use deck_core::tree::{NodeKind, RecordedCost, SessionTree};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CursorIcon, FontId, Frame, Id, Margin, Modifiers, Order, Pos2, Rect,
    RichText, Sense, Sides, Stroke, StrokeKind, Ui, pos2, vec2,
};
pub(crate) use preview::PreviewState;

use crate::card;
use crate::chat_view::{Ink, paint as chat_paint};
use crate::confirm::{self, Dialog, Reply};
use crate::icons::Icon;
use crate::icons::{
    ICON_ARROW_RIGHT, ICON_BOOKMARK, ICON_COPY, ICON_CORNER_DOWN_RIGHT, ICON_CROSSHAIR,
    ICON_GIT_FORK, ICON_KEY_ROUND, ICON_LIST_TREE, ICON_MESSAGES_SQUARE, ICON_SCROLL_TEXT, ICON_X,
};
use crate::theme::{self, Palette};
use crate::tree_layout::{self, Filter, FuzzySearch, Layout, RowClass, SearchIndex};
use crate::widgets::{DraftButton, empty_state, menu_item, paint_icon};
use geometry::Geometry;
pub(crate) use geometry::{gutter, row_height, visible_range};
use keys::{label_ok, match_target, move_target, search_rows};
use paint::{class_color, class_icon, ink_of, paint_graph, paint_node};

/// Rows per page before the list has been measured.
const DEFAULT_PAGE: usize = 12;
/// The navigator keeps this share of the body width.
const NAVIGATOR_SHARE: f32 = 0.52;
/// The preview pane hides below this window width.
const PREVIEW_MIN_W: f32 = 900.0;

/// Right-hand column of a hovered card: compact label / fork / go-here icon buttons in the
/// age's place (32px each + 6px gaps).
const ACTIONS_W: f32 = 3.0 * 32.0 + 2.0 * 6.0;
/// The same on an `ask` result: one more button (re-answer).
const ACTIONS_W_ASK: f32 = 4.0 * 32.0 + 3.0 * 6.0;

fn actions_w(ask: bool) -> f32 {
    if ask { ACTIONS_W_ASK } else { ACTIONS_W }
}

pub enum TreeAction {
    Close,
    /// Fork the session at `entry` (already resolved: a user node forks at its parent).
    Fork {
        entry: String,
        what: String,
    },
    Navigate {
        entry: String,
        /// The target is a user prompt: omp moves to its parent and returns the text to the editor.
        user: bool,
        /// What omp does with the branch being left.
        summary: BranchSummary,
    },
    /// Set (`Some`) or remove (`None`) the label of `entry`.
    SetLabel {
        entry: String,
        label: Option<String>,
    },
    /// Re-answer the `ask` result `entry`: omp asks the question again in the session's tab.
    Reanswer {
        entry: String,
    },
    Copy {
        text: String,
        what: &'static str,
    },
    /// A keyboard shortcut that cannot run right now.
    Notice(String),
    /// Cancel omp's running branch summary (Esc / Cancel while summarizing).
    CancelSummary,
    /// Open a link the preview's reader clicked.
    OpenUrl(String),
}

pub struct TreeInput<'a> {
    pub tree: Option<&'a Arc<SessionTree>>,
    pub revision: u64,
    pub error: Option<&'a str>,
    /// The session file the preview reads entries from.
    pub session: Option<&'a std::path::Path>,
    /// The live leaf entry id, when the session has a control link.
    pub live_leaf: Option<&'a str>,
    /// `Ok` = Go here is available; `Err` = why it is not.
    pub navigate: Result<(), &'a str>,
    /// `Ok` = Fork is available; `Err` = why it is not.
    pub fork: Result<(), &'a str>,
    /// omp's `branchSummary.enabled`: Go here asks whether to summarize the branch it leaves.
    pub summary_prompt: bool,
    /// `Ok` = labels can be edited; `Err` = why not.
    pub label: Result<(), &'a str>,
    /// `Ok` = an `ask` result can be re-answered in the session's tab; `Err` = why not.
    pub reanswer: Result<(), &'a str>,
    /// What the session is busy with (shown as progress), if anything.
    pub busy: Option<&'a str>,
    /// The busy state is omp writing a branch summary: Esc / Cancel aborts it.
    pub summarizing: bool,
    pub now: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    None,
    /// Center the leaf row.
    Leaf,
    /// Bring the selected row into view.
    Selected,
}

struct Cached {
    revision: u64,
    leaf: Option<usize>,
    filter: Filter,
    layout: Layout,
    /// Prefix sums of row top offsets (`rows.len() + 1` entries); rebuilt with the layout.
    offsets: Vec<f32>,
}

struct Hits {
    query: String,
    revision: u64,
    filter: Filter,
    leaf: Option<usize>,
    tokens: Vec<String>,
    rows: Vec<bool>,
}

pub struct TreeState {
    pub title: String,
    filter: Filter,
    /// Set once the user picks a filter (so omp's `treeFilterMode` initial value only applies).
    pub filter_touched: bool,
    search: String,
    cache: Option<Cached>,
    hits: Option<Hits>,
    /// Selected node id.
    selected: Option<String>,
    want: Want,
    /// The view follows the leaf while the session grows.
    pinned: bool,
    last_offset: f32,
    settle: u8,
    /// Rows the list shows at once (PgUp / PgDn / ← / → step); measured by the last frame.
    page: usize,
    /// Prepared search texts of the tree revision in `index_revision`.
    index: SearchIndex,
    index_revision: Option<u64>,
    focus_search: bool,
    /// The label being edited.
    label: Option<LabelEdit>,
    /// The "Summarize branch?" question of a Go here.
    summary: Option<SummaryDialog>,
    /// The search box had focus at the end of the previous frame: egui drops focus on Esc
    /// before the app sees the key, so this is what tells "Esc in the box" from "Esc".
    search_focused: bool,
    galleys: GalleyCache,
    search_id: Id,
    scratch: Vec<RowUi>,
    long: Vec<u32>,
    preview: PreviewState,
}

impl TreeState {
    pub fn new(title: String) -> TreeState {
        TreeState {
            title,
            filter: Filter::default(),
            filter_touched: false,
            search: String::new(),
            cache: None,
            hits: None,
            selected: None,
            want: Want::Leaf,
            pinned: true,
            last_offset: 0.0,
            settle: 0,
            page: DEFAULT_PAGE,
            index: SearchIndex::default(),
            index_revision: None,
            focus_search: false,
            label: None,
            summary: None,
            search_focused: false,
            search_id: Id::unique("tree-search"),
            galleys: GalleyCache::default(),
            scratch: Vec::new(),
            long: Vec::new(),
            preview: PreviewState::default(),
        }
    }

    /// omp's `treeFilterMode` initial value; only while the user has not picked a filter.
    pub fn set_initial_filter(&mut self, filter: Filter) {
        if !self.filter_touched && self.filter != filter {
            self.filter = filter;
            self.cache = None;
            self.want = Want::Selected;
        }
    }
}

/// What a row's text layout depends on besides the node and the width.
struct GalleyKey<'a> {
    revision: u64,
    ppp: u32,
    theme: [Color32; 6],
    font: UiFont,
    tokens: &'a [String],
}

/// Laid-out row texts, so a frame does not rebuild a `LayoutJob` per visible row. Valid for one
/// (tree revision, pixel ratio, theme, search tokens); keyed per row by (node, on path, width).
#[derive(Default)]
struct GalleyCache {
    valid: bool,
    revision: u64,
    ppp: u32,
    font: UiFont,
    theme: [Color32; 6],
    tokens: Vec<String>,
    map: HashMap<(usize, bool, u32), Arc<egui::Galley>>,
}

/// More laid-out rows than a long scroll through a big tree needs.
const GALLEY_CAP: usize = 4096;

impl GalleyCache {
    fn sync(&mut self, k: GalleyKey<'_>) {
        if self.valid
            && self.revision == k.revision
            && self.ppp == k.ppp
            && self.font == k.font
            && self.theme == k.theme
            && self.tokens == k.tokens
        {
            return;
        }
        self.valid = true;
        self.revision = k.revision;
        self.ppp = k.ppp;
        self.font = k.font;
        self.theme = k.theme;
        self.tokens = k.tokens.to_vec();
        self.map.clear();
    }

    fn row(
        &mut self,
        key: (usize, bool, u32),
        build: impl FnOnce() -> Arc<egui::Galley>,
    ) -> Arc<egui::Galley> {
        if let Some(g) = self.map.get(&key) {
            return Arc::clone(g);
        }
        if self.map.len() >= GALLEY_CAP {
            self.map.clear();
        }
        let g = build();
        self.map.insert(key, Arc::clone(&g));
        g
    }
}

/// What the pointer did on one visible row this frame.
struct RowUi {
    row: usize,
    hovered: bool,
}

/// Selection after `delta` steps through `len` rows, clamped; no selection starts at the edge.
pub fn step_row(current: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    Some(match current {
        Some(c) => c.saturating_add_signed(delta).min(last),
        None if delta < 0 => last,
        None => 0,
    })
}

// ------------------------------------------------------------------ show

pub fn show(ctx: &egui::Context, st: &mut TreeState, input: &TreeInput<'_>) -> Vec<TreeAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();

    egui::Area::new(Id::unique("tree-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() {
                out.push(TreeAction::Close);
            }
        });

    sync(st, input);
    keyboard(ctx, st, input, &mut out);

    let size = vec2(
        (screen.width() * 0.74)
            .clamp(680.0, 1500.0)
            .min((screen.width() - 48.0).max(560.0)),
        (screen.height() * 0.82)
            .clamp(440.0, 1050.0)
            .min((screen.height() - 48.0).max(360.0)),
    );

    egui::Window::new("tree-window")
        .id(Id::unique("tree-window"))
        .title_bar(false)
        .frame(Frame::NONE)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(560.0, 360.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| {
            card::chrome(ui, Margin::ZERO, pal.bar, |ui| {
                body(ui, st, input, &mut out)
            });
        });
    st.search_focused = ctx.memory(|m| m.has_focus(st.search_id));
    summary_dialog(ctx, st, &mut out);
    preview::poll(st, input, ctx);
    out
}

/// Rebuild the cached layout and search hits when their inputs changed.
fn sync(st: &mut TreeState, input: &TreeInput<'_>) {
    let Some(tree) = input.tree else {
        st.cache = None;
        st.hits = None;
        return;
    };
    let leaf = input
        .live_leaf
        .and_then(|id| tree.index.get(id).copied())
        .or(tree.file_leaf);
    let stale = st
        .cache
        .as_ref()
        .is_none_or(|c| c.revision != input.revision || c.leaf != leaf || c.filter != st.filter);
    if stale {
        let grown = st
            .cache
            .as_ref()
            .is_some_and(|c| c.revision != input.revision);
        let layout = tree_layout::build(tree, leaf, st.filter);
        let mut offsets = Vec::with_capacity(layout.rows.len() + 1);
        offsets.push(0.0);
        let mut y = 0.0;
        for row in &layout.rows {
            y += geometry::row_slot(row.class);
            offsets.push(y);
        }
        let first = st.cache.is_none();
        st.cache = Some(Cached {
            revision: input.revision,
            leaf,
            filter: st.filter,
            layout,
            offsets,
        });
        let Some(c) = &st.cache else {
            return;
        };
        // A filter that hides the selection leaves it on the nearest listed ancestor.
        let anchor = st
            .selected
            .as_ref()
            .and_then(|id| tree.index.get(id.as_str()).copied())
            .and_then(|i| c.layout.nearest_listed(tree, i));
        match anchor {
            Some(i) if !first => {
                let id = tree.nodes[i].id.as_str();
                if st.selected.as_deref() != Some(id) {
                    st.selected = Some(id.to_owned());
                }
            }
            _ => {
                // omp opens `/tree` on the leaf.
                st.selected = c
                    .layout
                    .leaf_row
                    .map(|r| tree.nodes[c.layout.rows[r].node].id.clone());
                if first {
                    st.want = Want::Leaf;
                }
            }
        }
        if grown && st.pinned && !first {
            st.want = Want::Leaf;
            if let Some(r) = c.layout.leaf_row {
                st.selected = Some(tree.nodes[c.layout.rows[r].node].id.clone());
            }
        } else if !first && st.want == Want::None {
            st.want = Want::Selected;
        }
    }
    let Some(c) = &st.cache else {
        return;
    };
    let current = st.hits.as_ref().is_some_and(|h| {
        h.query == st.search && h.revision == c.revision && h.filter == c.filter && h.leaf == c.leaf
    });
    if !current {
        if st.index_revision != Some(c.revision) {
            st.index.clear();
            st.index_revision = Some(c.revision);
        }
        let tokens = tree_layout::search_tokens(&st.search);
        let search = FuzzySearch::new(&st.search);
        let rows = if search.is_empty() {
            Vec::new()
        } else {
            tree_layout::search_matches(tree, &c.layout, &search, &mut st.index)
        };
        st.hits = Some(Hits {
            query: st.search.clone(),
            revision: c.revision,
            filter: c.filter,
            leaf: c.leaf,
            tokens,
            rows,
        });
        follow_matches(st, tree);
    }
}

fn selected_row(st: &TreeState, tree: &SessionTree) -> Option<usize> {
    let c = st.cache.as_ref()?;
    let i = *tree.index.get(st.selected.as_deref()?)?;
    c.layout.row_of_node(i)
}

fn keyboard(
    ctx: &egui::Context,
    st: &mut TreeState,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let Some(tree) = input.tree else {
        if !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, egui::Key::Escape))
        {
            out.push(TreeAction::Close);
        }
        return;
    };
    // The summary question owns the keys while it is open.
    if st.summary.is_some() {
        return;
    }
    // The label field: Enter saves, Esc cancels; everything else is typing.
    if st.label.is_some() {
        let (esc, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, egui::Key::Escape),
                i.consume_key(Modifiers::NONE, egui::Key::Enter),
            )
        });
        if esc {
            st.label = None;
        } else if enter {
            finish_label(st, input, out);
        }
        return;
    }
    let searching = st.search_focused || ctx.memory(|m| m.has_focus(st.search_id));
    let menu_open = egui::Popup::is_any_open(ctx);
    // Like omp, the label key is a command only while no search is typed.
    let label_ok = label_ok(searching, &st.search);
    let keys = ctx.input_mut(|i| keys::read(i, searching, menu_open, label_ok));
    let (esc, enter, fork, slash) = (keys.esc, keys.enter, keys.fork, keys.slash);
    if keys.close {
        out.push(TreeAction::Close);
        return;
    }
    if esc {
        if input.summarizing {
            // omp re-shows the tree: cancel the summary but stay in the window.
            out.push(TreeAction::CancelSummary);
            return;
        }
        if searching {
            // First Esc leaves the search box (clearing it); the next one closes the view.
            st.search.clear();
            st.search_focused = false;
            ctx.memory_mut(|m| m.surrender_focus(st.search_id));
        } else {
            out.push(TreeAction::Close);
        }
        return;
    }
    if slash {
        st.focus_search = true;
    }
    let next_filter = keys
        .filter
        .or_else(|| keys.cycle.map(|d| st.filter.cycled(d)));
    if let Some(f) = next_filter
        && f != st.filter
    {
        set_filter(st, f);
        ctx.request_repaint();
    }
    let Some(c) = &st.cache else {
        return;
    };
    let cur = selected_row(st, tree);
    // While a query has matches only they can be selected, like omp's filtered list.
    let target = keys.mv.and_then(|mv| {
        search_rows(st)
            .and_then(|rows| match_target(rows, cur, mv, st.page))
            .or_else(|| move_target(tree, &c.layout, cur, mv, st.page))
    });
    if let Some(r) = target
        && Some(r) != cur
    {
        select_row(st, tree, r);
        st.want = Want::Selected;
    }
    if keys.copy
        && let Some(r) = selected_row(st, tree)
    {
        copy_row(st, tree, r, out);
    }
    if enter && let Some(r) = selected_row(st, tree) {
        go_here(st, tree, input, r, keys.summarize, out);
    }
    if fork && let Some(r) = selected_row(st, tree) {
        fork_here(st, tree, input, r, out);
    }
    if keys.label
        && let Some(r) = selected_row(st, tree)
    {
        label_here(st, tree, input, r, out);
    }
}

/// A user-picked filter (not the initial value): it sticks.
fn set_filter(st: &mut TreeState, f: Filter) {
    st.filter = f;
    st.filter_touched = true;
    st.want = Want::Selected;
}

/// After the query (or what it runs over) changed: a selection the query excludes moves to the
/// nearest match. Nothing moves when no row matches.
fn follow_matches(st: &mut TreeState, tree: &SessionTree) {
    let cur = selected_row(st, tree);
    let Some(rows) = search_rows(st) else {
        return;
    };
    if cur.is_some_and(|r| rows.get(r).copied().unwrap_or(false)) {
        return;
    }
    let Some(r) = keys::nearest_match(rows, cur) else {
        return;
    };
    select_row(st, tree, r);
    st.want = Want::Selected;
}

fn select_row(st: &mut TreeState, tree: &SessionTree, r: usize) {
    let Some(c) = &st.cache else {
        return;
    };
    let Some(row) = c.layout.rows.get(r) else {
        return;
    };
    st.selected = Some(tree.nodes[row.node].id.clone());
    // Resting on the leaf re-engages the live follow; moving elsewhere stops it.
    st.pinned = row.is_leaf;
}

/// Why Go here is unavailable for row `r`, if it is.
fn go_blocked<'a>(layout: &Layout, input: &TreeInput<'a>, r: usize) -> Option<&'a str> {
    if let Err(why) = input.navigate {
        return Some(why);
    }
    let row = &layout.rows[r];
    (layout.leaf_node == Some(row.node)).then_some("Already at this point")
}

/// Why Fork is unavailable for row `r`, if it is.
fn fork_blocked<'a>(
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'a>,
    r: usize,
) -> Option<&'a str> {
    let node = layout.rows[r].node;
    if !tree.is_forkable(node) {
        return Some(if matches!(tree.nodes[node].kind, NodeKind::User) {
            "Nothing comes before the first prompt, so there is nothing to fork"
        } else {
            "Only messages can be forked"
        });
    }
    input.fork.err()
}

/// What "Go here" does about a branch summary.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GoPlan {
    /// Ask "No summary / Summarize / Summarize with custom prompt".
    Ask,
    Now(BranchSummary),
}

/// omp: Shift+Enter pre-answers "Summarize" and skips the question; otherwise the question is
/// asked only while `branchSummary.enabled`.
fn go_plan(prompt_enabled: bool, shift: bool) -> GoPlan {
    if shift {
        GoPlan::Now(BranchSummary::Summarize)
    } else if prompt_enabled {
        GoPlan::Ask
    } else {
        GoPlan::Now(BranchSummary::None)
    }
}

/// The three answers of omp's "Summarize branch?" selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryChoice {
    None,
    Summarize,
    Custom,
}

impl SummaryChoice {
    const ALL: [SummaryChoice; 3] = [
        SummaryChoice::None,
        SummaryChoice::Summarize,
        SummaryChoice::Custom,
    ];

    fn title(self) -> &'static str {
        match self {
            SummaryChoice::None => "No summary",
            SummaryChoice::Summarize => "Summarize",
            SummaryChoice::Custom => "Summarize with custom prompt…",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            SummaryChoice::None => "Leave the branch behind as it is.",
            SummaryChoice::Summarize => "omp writes a summary of the branch and carries it over.",
            SummaryChoice::Custom => "Say what the summary should focus on.",
        }
    }

    /// `delta` steps through the list, stopping at both ends.
    fn step(self, delta: isize) -> SummaryChoice {
        let at = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Self::ALL[at.saturating_add_signed(delta).min(Self::ALL.len() - 1)]
    }
}

/// A Go here waiting for its answer to "Summarize branch?".
struct SummaryDialog {
    entry: String,
    user: bool,
    choice: SummaryChoice,
    instructions: String,
    /// The instructions field takes the caret the next time it is drawn.
    focus_editor: bool,
    /// The chosen answer takes the keyboard focus the next time it is drawn.
    refocus: bool,
    editor: Id,
}

impl SummaryDialog {
    fn new(entry: String, user: bool) -> SummaryDialog {
        SummaryDialog {
            entry,
            user,
            choice: SummaryChoice::None,
            instructions: String::new(),
            focus_editor: false,
            refocus: true,
            editor: Id::unique("tree-summary-instructions"),
        }
    }

    fn choose(&mut self, choice: SummaryChoice) {
        self.focus_editor = choice == SummaryChoice::Custom && self.choice != choice;
        self.refocus = true;
        self.choice = choice;
    }

    /// What to send. Blank instructions are omp's default summary prompt.
    fn summary(&self) -> BranchSummary {
        match self.choice {
            SummaryChoice::None => BranchSummary::None,
            SummaryChoice::Summarize => BranchSummary::Summarize,
            SummaryChoice::Custom => match self.instructions.trim() {
                "" => BranchSummary::Summarize,
                text => BranchSummary::Custom(text.to_owned()),
            },
        }
    }
}

/// The label field's state.
struct LabelEdit {
    entry: String,
    /// The label the entry has (blank = none).
    original: String,
    text: String,
    /// The field takes the caret the next time it is drawn.
    focus: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum LabelEnd {
    /// Nothing to send: the text is what the entry already has.
    Unchanged,
    /// The new label; `None` removes it.
    Set(Option<String>),
}

impl LabelEdit {
    fn new(entry: String, current: Option<&str>) -> LabelEdit {
        let current = current.unwrap_or("").to_owned();
        LabelEdit {
            entry,
            text: current.clone(),
            original: current,
            focus: true,
        }
    }

    /// Enter: a blank field removes the label, like omp.
    fn finish(&self) -> LabelEnd {
        let new = self.text.trim();
        if new == self.original.trim() {
            return LabelEnd::Unchanged;
        }
        LabelEnd::Set((!new.is_empty()).then(|| new.to_owned()))
    }
}

/// Enter in the label field (or Save): send the label, or just close when nothing changed.
fn finish_label(st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let Some(edit) = st.label.take() else {
        return;
    };
    let LabelEnd::Set(label) = edit.finish() else {
        return;
    };
    match input.label {
        Ok(()) => out.push(TreeAction::SetLabel {
            entry: edit.entry,
            label,
        }),
        Err(why) => {
            out.push(TreeAction::Notice(why.to_owned()));
            st.label = Some(edit);
        }
    }
}

/// Open the label field on row `r`.
fn label_here(
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    if let Err(why) = input.label {
        out.push(TreeAction::Notice(why.to_owned()));
        return;
    }
    let Some(row) = st.cache.as_ref().and_then(|c| c.layout.rows.get(r)) else {
        return;
    };
    let id = tree.nodes[row.node].id.clone();
    let current = tree.labels.get(&id).map(String::as_str);
    st.label = Some(LabelEdit::new(id.clone(), current));
}

/// Why "Re-answer…" is unavailable for row `r`, if it is.
fn reanswer_blocked<'a>(
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'a>,
    r: usize,
) -> Option<&'a str> {
    if !tree_layout::is_ask_result(&tree.nodes[layout.rows[r].node]) {
        return Some("Only the answer to an ask can be re-answered");
    }
    input.reanswer.err()
}

fn reanswer_here(
    st: &TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    match reanswer_blocked(tree, &c.layout, input, r) {
        Some(why) => out.push(TreeAction::Notice(why.to_owned())),
        None => out.push(TreeAction::Reanswer {
            entry: tree.nodes[c.layout.rows[r].node].id.clone(),
        }),
    }
}

/// Go to row `r`: now, or after asking about a branch summary (`shift` = Shift+Enter).
fn go_here(
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    shift: bool,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    // The row is dimmed out by the query: not part of the list.
    if keys::excluded_by_search(st, r) {
        return;
    }
    if let Some(why) = go_blocked(&c.layout, input, r) {
        out.push(TreeAction::Notice(why.to_owned()));
        return;
    }
    let n = &tree.nodes[c.layout.rows[r].node];
    let (entry, user) = (n.id.clone(), matches!(n.kind, NodeKind::User));
    match go_plan(input.summary_prompt, shift) {
        GoPlan::Ask => st.summary = Some(SummaryDialog::new(entry, user)),
        GoPlan::Now(summary) => out.push(TreeAction::Navigate {
            entry,
            user,
            summary,
        }),
    }
}

fn fork_here(
    st: &TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    match fork_blocked(tree, &c.layout, input, r) {
        Some(why) => out.push(TreeAction::Notice(why.to_owned())),
        None => {
            let node = c.layout.rows[r].node;
            if let Some(entry) = tree.fork_target(node) {
                out.push(TreeAction::Fork {
                    entry: entry.to_owned(),
                    what: tree_layout::fork_hint(&tree.nodes[node].kind).to_owned(),
                });
            }
        }
    }
}

/// The preview pane's Copy: the cached entry content, or the row text while nothing is cached.
fn copy_row(st: &TreeState, tree: &SessionTree, r: usize, out: &mut Vec<TreeAction>) {
    let Some(c) = &st.cache else {
        return;
    };
    let Some(row) = c.layout.rows.get(r) else {
        return;
    };
    let node = &tree.nodes[row.node];
    let text = st
        .preview
        .cached_copy(&node.id)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| node.text.clone());
    if !text.is_empty() {
        out.push(TreeAction::Copy {
            text,
            what: "entry text",
        });
    }
}

// ------------------------------------------------------------------ layout of the window

fn body(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
    ui.spacing_mut().scroll = egui::style::ScrollStyle::floating();

    Frame::NONE
        .inner_margin(Margin {
            left: 22,
            right: 18,
            top: 14,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            header(ui, st, input, out);
        });
    let y = ui.cursor().top();
    card::header_rule(
        ui,
        ui.max_rect().left(),
        ui.max_rect().right(),
        y,
        Color32::TRANSPARENT,
    );
    Frame::NONE
        .inner_margin(Margin {
            left: 22,
            right: 18,
            top: 0,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            toolbar(ui, st, input);
            let y = ui.cursor().top();
            ui.painter().hline(
                ui.max_rect().left()..=ui.max_rect().right(),
                y,
                egui::Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            );
            busy_strip(ui, st, input, out);
            rest(ui, st, tree_opt(input), input, out);
        });
}

fn tree_opt<'a>(input: &TreeInput<'a>) -> Option<&'a std::sync::Arc<SessionTree>> {
    input.tree
}

/// Everything below the toolbar: the columns (or a status), then the key-hint footer strip.
fn rest(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: Option<&std::sync::Arc<SessionTree>>,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    // The body and the footer strip split exactly the remaining rect: allocating more than
    // that (e.g. item spacing on top) would grow the resizable window every frame.
    let foot_h = 34.0;
    let avail = ui.available_rect_before_wrap();
    let split = (avail.bottom() - foot_h).max(avail.top() + 120.0);
    ui.allocate_rect(avail, Sense::hover());
    let mut main = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(avail.min, pos2(avail.right(), split)))
            .layout(egui::Layout::top_down(Align::Min)),
    );
    let mut foot = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(pos2(avail.left(), split), avail.max))
            .layout(egui::Layout::top_down(Align::Min)),
    );
    footer(&mut foot);
    let Some(tree) = tree else {
        centered_status(&mut main, input);
        return;
    };
    let empty = st.cache.as_ref().is_none_or(|c| c.layout.rows.is_empty());
    if empty {
        let entries = st.cache.as_ref().map_or(0, |c| c.layout.stats.entries);
        let (title, sub) = match st.filter {
            _ if entries == 0 => ("Nothing to show", "This session has no entries yet."),
            Filter::Labeled => (
                "No labeled entries",
                "Labels you add with /label show up here.",
            ),
            _ => (
                "Nothing matches this filter",
                "Alt+A shows every entry, Alt+D the default view.",
            ),
        };
        empty_state(&mut main, ICON_LIST_TREE, pal.weak, title, sub);
        return;
    }
    columns(&mut main, st, tree, input, out);
}

fn centered_status(ui: &mut Ui, input: &TreeInput<'_>) {
    let pal = Palette::of_ui(ui);
    match input.error {
        Some(e) => empty_state(ui, ICON_LIST_TREE, pal.danger, "Cannot read the session", e),
        None => {
            ui.add_space(ui.available_height() * 0.35);
            ui.vertical_centered(|ui| {
                ui.add(crate::widgets::Spin::new(22.0, pal.accent));
                ui.add_space(8.0);
                ui.label(RichText::new("Reading the session…").color(pal.weak));
            });
        }
    }
}

/// Header band: icon tile, title + session subtitle, stat badges, close.
fn header(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    let ink = ink_of(ui, 12.0);
    // Reserve the band rect FIRST (painted below): content lays out inside it afterwards. The
    // band spans the window's full width; the Frame's margins only pad the content.
    let avail = ui.available_size();
    let band = Rect::from_min_max(
        pos2(ui.max_rect().left() - 22.0, ui.cursor().top()),
        pos2(
            ui.max_rect().right() + 18.0,
            ui.cursor().top() + 44.0 + 26.0,
        ),
    );
    let bg = chat_paint::reserve(ui.painter());
    ui.allocate_space(vec2(avail.x, band.height()));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(band.shrink2(vec2(22.0, 13.0))));
    let resp = Sides::new().height(44.0).show(
        &mut child,
        |ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            card::icon_tile(ui, 34.0, ICON_LIST_TREE, Color32::TRANSPARENT, true);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    theme::strong("Session tree")
                        .size(17.5)
                        .color(pal.text_strong),
                );
                if !st.title.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(&st.title).color(pal.weak).size(12.5))
                            .truncate(),
                    );
                }
            });
        },
        |ui| {
            // Right-to-left: the first item sits at the far right.
            ui.spacing_mut().item_spacing.x = 8.0;
            if card::close_button_sized(ui, 30.0, 18.0).clicked() {
                out.push(TreeAction::Close);
            }
            ui.add_space(6.0);
            if let Some(c) = &st.cache {
                let s = c.layout.stats;
                stat_badge(ui, &ink, None, &format!("{} entries", fmt_count(s.entries)));
                if s.tips > 1 {
                    stat_badge(
                        ui,
                        &ink,
                        Some(ink.b),
                        &format!("{} branches", fmt_count(s.tips)),
                    );
                }
                if input.live_leaf.is_some() {
                    live_badge(ui, &ink);
                }
            }
        },
    );
    let _ = &resp;
    let pts = chat_paint::slant_band(band, 18.0);
    let (a, b) = ink.head();
    ui.painter()
        .set(bg, head_fill_shape(&pts, &ink, &band, a, b));
    chat_paint::hairline(
        ui.painter(),
        &ink,
        band.left(),
        band.right(),
        band.bottom(),
        0.7,
    );
}

/// The header band's fill as a settable shape (mirrors [`chat_paint::fill_h`]'s gradient).
fn head_fill_shape(pts: &[Pos2], ink: &Ink, band: &Rect, a: Color32, b: Color32) -> egui::Shape {
    if theme::flat() {
        return egui::Shape::convex_polygon(pts.to_vec(), ink.p.raised, Stroke::NONE);
    }
    let span = band.width().max(1.0);
    let col = |p: Pos2| a.lerp_to_gamma(b, ((p.x - band.left()) / span).clamp(0.0, 1.0));
    let c = band.center();
    let mut m = egui::Mesh::default();
    m.colored_vertex(c, col(c));
    for p in pts {
        m.colored_vertex(*p, col(*p));
    }
    let n = pts.len() as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    egui::Shape::Mesh(m.into())
}

/// One washed stat badge in the header.
fn stat_badge(ui: &mut Ui, ink: &Ink, tint: Option<Color32>, text: &str) {
    let pal = ink.p;
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::label_font(11.0), pal.text);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 22.0), Sense::hover());
    let p = ui.painter();
    let fill = match tint {
        Some(c) if !theme::flat() => ink.wash(c),
        _ => ink.chip(),
    };
    let edge = match tint {
        Some(c) => ink.edge(c),
        None => ink.line(),
    };
    chat_paint::boxed(
        p,
        rect,
        4,
        fill,
        crate::badge::outline(p.pixels_per_point(), edge),
    );
    p.galley(
        rect.center() - g.size() / 2.0,
        g,
        tint.map_or(pal.text, |c| ink.legible(c, fill, 3.0)),
    );
}

/// Green `● live` marker.
fn live_badge(ui: &mut Ui, ink: &Ink) {
    let pal = ink.p;
    let g = ui
        .painter()
        .layout_no_wrap("live".to_owned(), theme::label_font(11.0), pal.ok);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 22.0, 22.0), Sense::hover());
    theme::glow_dot(
        ui.painter(),
        pos2(rect.left() + 8.0, rect.center().y),
        3.5,
        pal.ok,
        theme::GLOW_REST,
    );
    ui.painter().galley(
        pos2(rect.left() + 15.0, rect.center().y - g.size().y / 2.0),
        g,
        pal.ok,
    );
}

/// Toolbar row under the header: filter tabs left, search field right.
fn toolbar(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>) {
    let focus = std::mem::take(&mut st.focus_search);
    let search_id = st.search_id;
    let search = &mut st.search;
    let mut filter = st.filter;
    Sides::new().height(34.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for f in Filter::ALL {
                if filter_tab(ui, f, filter == f).clicked() {
                    filter = f;
                }
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            search_field(ui, search, search_id, focus, input);
        },
    );
    if filter != st.filter {
        set_filter(st, filter);
    }
    ui.add_space(2.0);
}

/// A text filter tab; the active one is washed, accent-outlined, with a cut corner.
fn filter_tab(ui: &mut Ui, filter: Filter, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let ink = ink_of(ui, 12.0);
    let g = ui.painter().layout_no_wrap(
        filter.title().to_owned(),
        crate::theme::font(12.5),
        pal.text,
    );
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 22.0, 30.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() || on, 0.1);
    let p = ui.painter();
    if on {
        let pts = chat_paint::slant_band(rect, 10.0);
        let wash = if theme::flat() {
            pal.raised
        } else {
            ink.wash(ink.a)
        };
        crate::deco::fill(p, pts.clone(), wash);
        crate::deco::outline(p, pts, Stroke::new(1.0, ink.edge(ink.a)));
    } else {
        p.rect_filled(rect, 4, pal.raised.gamma_multiply(0.7 * hov));
    }
    let color = if on {
        if theme::flat() {
            pal.text_strong
        } else {
            ink.legible(ink.a, ink.wash(ink.a), 3.0)
        }
    } else {
        theme::mix(pal.weak, pal.text_strong, hov)
    };
    p.galley(rect.center() - g.size() / 2.0, g, color);
    resp.on_hover_ui(|ui| {
        ui.label(filter.hint());
        ui.label(
            RichText::new(format!(
                "Alt+{} · Ctrl+O cycles, Ctrl+Shift+O cycles back",
                filter.shortcut()
            ))
            .size(12.0)
            .color(pal.weak),
        );
    })
    .on_hover_cursor(CursorIcon::PointingHand)
}

fn search_field(ui: &mut Ui, search: &mut String, id: Id, focus: bool, input: &TreeInput<'_>) {
    let pal = Palette::of_ui(ui);
    let _ = input;
    if !search.is_empty() {
        let clear = ui.add(egui::Button::new(icon(ICON_X, 16.0, pal.weak)).frame(false));
        if clear.on_hover_text("Clear search").clicked() {
            search.clear();
        }
    }
    let edit = egui::TextEdit::singleline(search)
        .id(id)
        .hint_text("Search prompts, replies, labels…  (/)")
        .desired_width(270.0)
        .margin(vec2(10.0, 6.0));
    let r = ui.add(edit);
    if focus {
        r.request_focus();
    }
}

fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn icon(i: Icon, size: f32, color: Color32) -> RichText {
    crate::widgets::icon_text(i, size, color)
}

/// The body split: the navigator left and the preview pane right.
fn columns(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &Arc<SessionTree>,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    // The threshold is a window width; the body lost 40px of padding getting here.
    let wide = ui.available_width() + 40.0;
    let show_preview = wide >= PREVIEW_MIN_W;
    let top = ui.cursor().top();
    let left = ui.max_rect().left();
    let full = ui.available_size();
    let nav_w = if show_preview {
        (full.x * NAVIGATOR_SHARE).clamp(340.0, full.x - 320.0)
    } else {
        full.x
    };
    // Claim the whole body rect; the navigator and the pane lay out in explicit child rects.
    ui.allocate_exact_size(full, Sense::hover());
    navigator(
        ui,
        Rect::from_min_size(pos2(left, top), vec2(nav_w, full.y)),
        st,
        tree,
        input,
        out,
    );
    if !show_preview {
        return;
    }
    ui.painter().vline(
        left + nav_w,
        top..=top + full.y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
    );
    let mut pane = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(left + nav_w + 1.0, top),
                pos2(left + full.x, top + full.y),
            ))
            .layout(egui::Layout::top_down(Align::Min)),
    );
    preview::pane(&mut pane, st, tree, input, out);
}

/// The navigator column in `rect`: list head, the row list, and the legend strip at the bottom.
fn navigator(
    ui: &mut Ui,
    rect: Rect,
    st: &mut TreeState,
    tree: &Arc<SessionTree>,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let child = |ui: &mut Ui, r: Rect| {
        ui.new_child(
            egui::UiBuilder::new()
                .max_rect(r)
                .layout(egui::Layout::top_down(Align::Min)),
        )
    };
    let (head_h, foot_h) = (26.0, 30.0);
    list_head(&mut child(ui, rect), st);
    let list_rect = Rect::from_min_max(
        rect.min + vec2(0.0, head_h),
        pos2(
            rect.right(),
            (rect.bottom() - foot_h).max(rect.top() + head_h),
        ),
    );
    list(&mut child(ui, list_rect), st, tree, input, out);
    list_foot(
        &mut child(
            ui,
            Rect::from_min_max(pos2(rect.left(), list_rect.bottom()), rect.right_bottom()),
        ),
        st,
    );
}

/// `TURN TIMELINE` caption and the shown/total counts.
fn list_head(ui: &mut Ui, st: &TreeState) {
    let pal = Palette::of_ui(ui);
    Sides::new().height(26.0).show(
        ui,
        |ui| {
            ui.label(theme::section_caption("Turn timeline", pal.weak));
        },
        |ui| {
            if let Some(c) = &st.cache {
                let s = c.layout.stats;
                ui.label(
                    RichText::new(format!(
                        "{} shown / {}",
                        fmt_count(s.shown),
                        fmt_count(s.entries)
                    ))
                    .size(11.5)
                    .color(pal.weak),
                );
            }
        },
    );
    let y = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().left()..=ui.max_rect().right(),
        y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.7)),
    );
    ui.add_space(4.0);
}

/// Legend and fork-point count under the list.
fn list_foot(ui: &mut Ui, st: &TreeState) {
    let pal = Palette::of_ui(ui);
    let y = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().left()..=ui.max_rect().right(),
        y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.7)),
    );
    Sides::new().height(24.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            legend(ui, "active path", pal.accent);
            ui.add_space(8.0);
            // The first alternate lane's tint (lanes past it cycle through more hues).
            let alt = paint::lane_color(&ink_of(ui, 12.0), 1);
            legend(ui, "alternate", alt);
        },
        |ui| {
            if let Some(c) = &st.cache {
                let forks = c.layout.stats.forks;
                if forks > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} fork point{}",
                            fmt_count(forks),
                            if forks == 1 { "" } else { "s" }
                        ))
                        .size(11.5)
                        .color(pal.weak),
                    );
                }
            }
        },
    );
}

fn legend(ui: &mut Ui, text: &str, color: Color32) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(16.0, 12.0), Sense::hover());
    ui.painter().hline(
        rect.left()..=rect.right(),
        rect.center().y,
        Stroke::new(2.0, color),
    );
    ui.label(RichText::new(text).size(11.0).color(pal.weak));
}

/// One compact footer row of keycap hints; hints are dropped on narrow widths.
fn footer(ui: &mut Ui) {
    let pal = Palette::of_ui(ui);
    let hints: &[(&str, &str)] = &[
        ("↑ ↓", "select"),
        ("[ ]", "branch"),
        ("Enter", "go here"),
        ("⇧ Enter", "summarize"),
        ("F", "fork"),
        ("L", "label"),
        ("/", "search"),
    ];
    let y = ui.cursor().top() + 4.0;
    ui.painter().hline(
        ui.max_rect().left()..=ui.max_rect().right(),
        y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
    );
    ui.add_space(10.0);
    ui.spacing_mut().item_spacing.y = 0.0;
    Sides::new().height(24.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            // The right end must keep `Esc close / cancel`: measure it, then fit hints.
            let avail = ui.available_width() - tail_w(ui) - 16.0;
            let mut used = 0.0;
            for (key, what) in hints {
                let w = hint_w(ui, key, what);
                if used + w > avail {
                    break;
                }
                used += w + 12.0;
                hint(ui, key, what);
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            card::kbd(ui, "Esc");
            ui.label(RichText::new("close / cancel").size(11.0).color(pal.weak));
        },
    );
}

fn hint_w(ui: &Ui, key: &str, what: &str) -> f32 {
    let kg = ui.painter().layout_no_wrap(
        key.to_owned(),
        theme::label_font(10.5),
        Color32::TRANSPARENT,
    );
    let wg = ui.painter().layout_no_wrap(
        what.to_owned(),
        crate::theme::font(11.0),
        Color32::TRANSPARENT,
    );
    kg.size().x + 10.0 + 5.0 + wg.size().x
}

fn tail_w(ui: &Ui) -> f32 {
    hint_w(ui, "Esc", "close / cancel")
}

fn hint(ui: &mut Ui, key: &str, what: &str) {
    let pal = Palette::of_ui(ui);
    ui.spacing_mut().item_spacing.x = 5.0;
    card::kbd(ui, key);
    ui.label(RichText::new(what).size(11.0).color(pal.weak));
}

/// Progress of what the session is busy with; a running summary offers Cancel.
fn busy_strip(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let Some(text) = input.busy else {
        return;
    };
    let pal = Palette::of_ui(ui);
    let ink = ink_of(ui, 12.0);
    if input.summarizing {
        let resp = Frame::NONE
            .inner_margin(Margin::symmetric(22, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    ui.add(crate::widgets::Spin::new(16.0, pal.warn));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            RichText::new("Summarizing branch…")
                                .size(12.5)
                                .color(pal.text),
                        );
                        ui.label(
                            RichText::new("You can cancel without leaving the tree.")
                                .size(11.5)
                                .color(pal.weak),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if DraftButton::new("Cancel")
                            .min_width(76.0)
                            .show(ui)
                            .on_hover_text("Cancel the summary (Esc)")
                            .clicked()
                        {
                            out.push(TreeAction::CancelSummary);
                        }
                    });
                });
            });
        let rect = resp.response.rect.expand2(vec2(22.0, 8.0));
        ui.painter()
            .rect_filled(rect, 0, ink.wash(pal.warn).gamma_multiply(0.6));
        ui.painter().hline(
            rect.left()..=rect.right(),
            rect.top(),
            Stroke::new(1.0, ink.edge(pal.warn)),
        );
        let _ = st;
    } else {
        Frame::NONE
            .fill(pal.surface)
            .inner_margin(Margin::symmetric(22, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.add(crate::widgets::Spin::new(16.0, pal.accent));
                    ui.label(RichText::new(text).size(13.0).color(pal.text));
                });
            });
    }
}

/// "Summarize branch?" before a Go here, as a Drafted dialog.
fn summary_dialog(ctx: &egui::Context, st: &mut TreeState, out: &mut Vec<TreeAction>) {
    let Some(d) = st.summary.as_mut() else {
        return;
    };
    let editor = d.editor;
    // The arrow keys move the choice, unless the instructions field has the caret.
    if !ctx.memory(|m| m.has_focus(editor)) {
        let delta = ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, egui::Key::ArrowDown) {
                1
            } else if i.consume_key(Modifiers::NONE, egui::Key::ArrowUp) {
                -1
            } else {
                0
            }
        });
        if delta != 0 {
            let next = d.choice.step(delta);
            d.choose(next);
        }
    }
    let custom = d.choice == SummaryChoice::Custom;
    let reply = confirm::show_dialog_reply(
        ctx,
        &Dialog {
            id: "tree-summary",
            title: "Summarize branch?",
            subtitle: Some("The branch you leave can be summarized and carried to the new point."),
            width: 480.0,
            confirm_label: "Go here",
            confirm_enabled: true,
            enter_confirms: !custom,
            ctrl_enter_confirms: custom,
            icon: Some(ICON_SCROLL_TEXT),
            ..Dialog::default()
        },
        "Cancel",
        |ui| summary_body(ui, d, editor),
    );
    match reply {
        Some(Reply::Confirmed) => {
            if let Some(d) = st.summary.take() {
                out.push(TreeAction::Navigate {
                    summary: d.summary(),
                    entry: d.entry,
                    user: d.user,
                });
            }
        }
        // The Cancel button, Esc or a click outside: back to the tree.
        Some(_) => st.summary = None,
        None => {}
    }
}

fn summary_body(ui: &mut Ui, d: &mut SummaryDialog, editor: Id) {
    let pal = Palette::of_ui(ui);
    ui.spacing_mut().item_spacing.y = 4.0;
    for choice in SummaryChoice::ALL {
        let on = d.choice == choice;
        let title = RichText::new(choice.title()).size(13.5).color(if on {
            pal.text_strong
        } else {
            pal.text
        });
        let radio = ui.radio(on, title);
        if radio.has_focus() {
            // The arrow keys belong to this list (they would move egui's focus otherwise).
            ui.ctx().memory_mut(|m| {
                m.set_focus_lock_filter(
                    radio.id,
                    egui::EventFilter {
                        vertical_arrows: true,
                        ..egui::EventFilter::default()
                    },
                );
            });
        }
        if on && std::mem::take(&mut d.refocus) && choice != SummaryChoice::Custom {
            radio.request_focus();
        }
        if radio.clicked() && !on {
            d.choose(choice);
        }
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            ui.label(RichText::new(choice.hint()).size(12.0).color(pal.weak));
        });
        ui.add_space(4.0);
    }
    if d.choice == SummaryChoice::Custom {
        let field = ui.add(
            egui::TextEdit::multiline(&mut d.instructions)
                .id(editor)
                .desired_rows(5)
                .desired_width(f32::INFINITY)
                .hint_text("What should the summary focus on? Empty uses omp's default prompt."),
        );
        if std::mem::take(&mut d.focus_editor) {
            field.request_focus();
        }
    }
    ui.add_space(8.0);
}

// ------------------------------------------------------------------ the list

#[allow(clippy::too_many_lines)]
fn list(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &Arc<SessionTree>,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    let ink = ink_of(ui, 12.0);
    let Some(cached) = st.cache.as_ref() else {
        return;
    };
    let rows = cached.layout.rows.len();
    let (step, graph_w) = gutter(cached.layout.lanes);
    let total_h = cached.offsets.last().copied().unwrap_or(0.0) + 12.0;
    let want = std::mem::replace(&mut st.want, Want::None);
    let sel = selected_row(st, tree);
    let leaf_row = cached.layout.leaf_row;
    let target = match want {
        Want::Leaf => leaf_row.or(sel),
        Want::Selected => sel,
        Want::None => None,
    };
    let dim_hits = st.hits.as_ref().filter(|h| !h.tokens.is_empty());
    st.galleys.sync(GalleyKey {
        revision: input.revision,
        ppp: ui.ctx().pixels_per_point().to_bits(),
        theme: [
            pal.text,
            pal.text_strong,
            pal.weak,
            pal.warn,
            pal.ok,
            pal.accent,
        ],
        font: crate::theme::ui_font(),
        tokens: dim_hits.map_or(&[], |h| &h.tokens[..]),
    });

    let mut clicked: Option<usize> = None;
    let mut actions: Vec<(usize, RowCommand)> = Vec::new();
    let mut page_count = st.page;
    let mut hover_id: Option<Id> = None;
    let ink_c = ink.clone();
    let area = egui::ScrollArea::vertical()
        .id_salt("tree-scroll")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), total_h), Sense::hover());
            let Some(cached) = st.cache.as_ref() else {
                return;
            };
            let geo = Geometry::new(
                rect.left(),
                rect.top(),
                step,
                graph_w,
                rect.width(),
                &cached.offsets,
            );
            if let Some(r) = target {
                let row = geo.slot(r);
                let align = if want == Want::Leaf {
                    Some(Align::Center)
                } else {
                    None
                };
                ui.scroll_to_rect_animation(
                    row.expand2(vec2(0.0, geometry::row_slot(cached.layout.rows[r].class))),
                    align,
                    egui::style::ScrollAnimation::none(),
                );
            }
            let (first, last) = visible_range(&cached.offsets, viewport.min.y, viewport.max.y);
            let last = last.min(rows);
            let first = first.min(last);
            let hover_pos = ui.input(|i| i.pointer.hover_pos());
            st.scratch.clear();
            let card_left = rect.left() + geometry::card_left(&geo);
            for r in first..last {
                let rr = geo.slot(r);
                let row_id = Id::unique("tree-row").with(r);
                let resp = ui.interact(rr, row_id, Sense::click());
                let hovered = hover_pos
                    .is_some_and(|p| rr.contains(p) && ui.clip_rect().contains(p))
                    && !egui::Popup::is_any_open(ui.ctx());
                if resp.clicked() {
                    clicked = Some(r);
                }
                if let Some(c) = &st.cache {
                    let node = c.layout.rows[r].node;
                    row_menu(&resp, tree, &c.layout, input, r, node, &mut actions);
                }
                if hovered {
                    hover_id = Some(row_id);
                }
                let _ = card_left;
                st.scratch.push(RowUi { row: r, hovered });
            }
            page_count = last.saturating_sub(first).max(1);
            let Some(layout) = st.cache.as_ref().map(|c| &c.layout) else {
                return;
            };
            // Cards, then the graph, then nodes and card content on top.
            let painter = ui.painter().clone();
            let hover_row = st.scratch.iter().find(|u| u.hovered).map(|u| u.row);
            let ctx = ui.ctx().clone();
            for u in &st.scratch {
                let id = Id::unique("tree-row").with(u.row);
                let hov_t = ctx.animate_bool_with_time(id.with("hov"), u.hovered, 0.12);
                paint::paint_card(&painter, &ink_c, layout, &geo, u, sel == Some(u.row), hov_t);
            }
            paint_graph(&painter, &ink_c, layout, &geo, first, last, &mut st.long);
            let active = hover_row;
            for u in &st.scratch {
                let row = &layout.rows[u.row];
                let dim = match dim_hits {
                    Some(h) if !h.rows.get(u.row).copied().unwrap_or(false) => 0.32,
                    _ => 1.0,
                };
                paint_node(
                    &painter,
                    &ink_c,
                    row,
                    geo.at(u.row, row.lane),
                    row.children > 1,
                );
                let mut p = painter.clone();
                p.set_opacity(dim);
                let tokens: &[String] = dim_hits.map_or(&[], |h| &h.tokens);
                let cmd = paint_content(
                    ui,
                    &p,
                    &ink_c,
                    &ContentCtx {
                        tree,
                        layout,
                        input,
                        geo: &geo,
                        tokens,
                        show_actions: active == Some(u.row),
                    },
                    u,
                    &mut st.galleys,
                );
                if let Some(cmd) = cmd {
                    actions.push((u.row, cmd));
                }
            }
        });
    let offset = area.state.clamped_offset().y;
    st.page = page_count.max(1);
    let _ = hover_id;
    if want != Want::None {
        st.settle = 2;
    } else if st.settle > 0 {
        st.settle -= 1;
    } else if (offset - st.last_offset).abs() > 0.5 {
        // The user scrolled: stop following the leaf.
        st.pinned = false;
    }
    st.last_offset = offset;
    if let Some(r) = clicked {
        select_row(st, tree, r);
    }
    for (r, cmd) in actions {
        match cmd {
            RowCommand::Fork => {
                select_row(st, tree, r);
                fork_here(st, tree, input, r, out);
            }
            RowCommand::Go => {
                select_row(st, tree, r);
                go_here(st, tree, input, r, false, out);
            }
            RowCommand::Label => {
                select_row(st, tree, r);
                label_here(st, tree, input, r, out);
            }
            RowCommand::Reanswer => {
                select_row(st, tree, r);
                reanswer_here(st, tree, input, r, out);
            }
            RowCommand::CopyText(text) => out.push(TreeAction::Copy {
                text,
                what: "entry text",
            }),
            RowCommand::CopyId(text) => out.push(TreeAction::Copy {
                text,
                what: "entry id",
            }),
        }
    }
    current_button(ui, st, tree, area.inner_rect);
}

/// Floating "jump to the current point" button, shown when the leaf is off screen.
fn current_button(ui: &mut Ui, st: &mut TreeState, tree: &SessionTree, list: Rect) {
    let Some(c) = st.cache.as_ref() else {
        return;
    };
    let Some(leaf_row) = c.layout.leaf_row else {
        return;
    };
    let top = st.last_offset;
    let y = c.offsets.get(leaf_row).copied().unwrap_or(0.0);
    let h = c
        .offsets
        .get(leaf_row + 1)
        .copied()
        .unwrap_or(y + row_height(c.layout.rows[leaf_row].class));
    if y + (h - y) > top && y < top + list.height() {
        return;
    }
    let pal = Palette::of_ui(ui);
    let g = ui.painter().layout_no_wrap(
        "Current".to_owned(),
        crate::theme::font(12.5),
        theme::on_accent(pal),
    );
    let size = vec2(g.size().x + 40.0, 30.0);
    let rect = Rect::from_min_size(
        pos2(list.right() - size.x - 22.0, list.bottom() - size.y - 14.0),
        size,
    );
    let resp = ui.interact(rect, Id::unique("tree-current"), Sense::click());
    let hov = resp.hovered();
    let p = ui.painter();
    if !theme::flat() {
        p.rect_filled(
            rect.translate(vec2(0.0, 2.0)),
            15,
            Color32::from_black_alpha(60),
        );
    }
    theme::gradient_rounded(
        p,
        rect,
        15.0,
        pal.accent_a.gamma_multiply(if hov { 1.0 } else { 0.9 }),
        pal.accent_b.gamma_multiply(if hov { 1.0 } else { 0.9 }),
    );
    paint_icon(
        p,
        pos2(rect.left() + 16.0, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_CROSSHAIR,
        16.0,
        theme::on_accent(pal),
    );
    p.galley(
        pos2(rect.left() + 28.0, rect.center().y - g.size().y / 2.0),
        g,
        theme::on_accent(pal),
    );
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        st.want = Want::Leaf;
        st.pinned = true;
        st.selected = st.cache.as_ref().and_then(|c| {
            c.layout
                .leaf_row
                .map(|r| tree.nodes[c.layout.rows[r].node].id.clone())
        });
    }
}

enum RowCommand {
    Fork,
    Go,
    Label,
    Reanswer,
    CopyText(String),
    CopyId(String),
}

fn row_menu(
    resp: &egui::Response,
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'_>,
    r: usize,
    node: usize,
    out: &mut Vec<(usize, RowCommand)>,
) {
    theme::context_menu(resp, 230.0, |ui| {
        let n = &tree.nodes[node];
        let fork_off = fork_blocked(tree, layout, input, r);
        let it = menu_item(
            ui,
            ICON_GIT_FORK,
            tree_layout::fork_hint(&n.kind),
            None,
            fork_off.is_none(),
            false,
        );
        let it = match fork_off {
            Some(why) => it.on_disabled_hover_text(why),
            None => it,
        };
        if it.clicked() {
            out.push((r, RowCommand::Fork));
            ui.close();
        }
        let go_off = go_blocked(layout, input, r);
        let it = menu_item(
            ui,
            ICON_CORNER_DOWN_RIGHT,
            "Go here",
            Some("Enter"),
            go_off.is_none(),
            false,
        );
        let it = match go_off {
            Some(why) => it.on_disabled_hover_text(why),
            None => it,
        };
        if it.clicked() {
            out.push((r, RowCommand::Go));
            ui.close();
        }
        let label_off = input.label.err();
        let it = menu_item(
            ui,
            ICON_BOOKMARK,
            "Edit label…",
            Some("L"),
            label_off.is_none(),
            false,
        );
        let it = match label_off {
            Some(why) => it.on_disabled_hover_text(why),
            None => it,
        };
        if it.clicked() {
            out.push((r, RowCommand::Label));
            ui.close();
        }
        if tree_layout::is_ask_result(n) {
            let off = reanswer_blocked(tree, layout, input, r);
            let it = menu_item(
                ui,
                ICON_MESSAGES_SQUARE,
                "Re-answer…",
                None,
                off.is_none(),
                false,
            );
            let it = match off {
                Some(why) => it.on_disabled_hover_text(why),
                None => it,
            };
            if it.clicked() {
                out.push((r, RowCommand::Reanswer));
                ui.close();
            }
        }
        ui.separator();
        if menu_item(ui, ICON_COPY, "Copy text", None, !n.text.is_empty(), false).clicked() {
            out.push((r, RowCommand::CopyText(n.text.clone())));
            ui.close();
        }
        if menu_item(ui, ICON_KEY_ROUND, "Copy entry id", None, true, false).clicked() {
            out.push((r, RowCommand::CopyId(n.id.clone())));
            ui.close();
        }
    });
}

// ------------------------------------------------------------------ painting rows

struct ContentCtx<'a> {
    tree: &'a SessionTree,
    layout: &'a Layout,
    input: &'a TreeInput<'a>,
    geo: &'a Geometry<'a>,
    tokens: &'a [String],
    show_actions: bool,
}

/// Glyph, text, pills, age or action buttons of one row.
fn paint_content(
    ui: &mut Ui,
    p: &egui::Painter,
    ink: &Ink,
    cx: &ContentCtx<'_>,
    u: &RowUi,
    gc: &mut GalleyCache,
) -> Option<RowCommand> {
    let pal = ink.p;
    let row = &cx.layout.rows[u.row];
    let node = &cx.tree.nodes[row.node];
    let card = cx.geo.card(u.row, row.class);
    let cy = card.center().y;
    let off_path = !row.on_path;
    let class_col = class_color(ink, row.class);
    let tone = |c: Color32| {
        if off_path {
            theme::mix(c, pal.weak, 0.3)
        } else {
            c
        }
    };
    let tool = row.class == RowClass::Tool;

    // Role glyph.
    let gx = card.left() + if tool { 13.0 } else { 16.0 };
    paint_icon(
        p,
        pos2(gx, cy),
        Align2::CENTER_CENTER,
        class_icon(row.class),
        if tool { 13.0 } else { 15.0 },
        tone(class_col),
    );
    let text_left = gx + if tool { 12.0 } else { 15.0 };

    // Right column: hover icons, or the age (laid out once, painted below). Pills sit left of it.
    let right = card.right() - 10.0;
    let ask = tree_layout::is_ask_result(node);
    let age = match (cx.show_actions, row.ts) {
        (false, Some(ts)) => Some(p.layout_no_wrap(
            tree_layout::age(cx.input.now, ts),
            ink.label(11.0),
            pal.weak,
        )),
        _ => None,
    };
    let right_side = if cx.show_actions {
        actions_w(ask) + 6.0
    } else {
        age.as_ref().map_or(0.0, |g| g.size().x + 8.0)
    };
    let mut pill_right = right - right_side;
    if row.is_leaf && !tool {
        let w = pill(p, ink, pill_right, cy, "you are here", PillStyle::Solid);
        pill_right -= w + 6.0;
    }
    if row.children > 1 && !tool {
        let txt = format!("{} branches", row.children);
        let w = pill(p, ink, pill_right, cy, &txt, PillStyle::Soft(ink.b));
        pill_right -= w + 6.0;
    }
    if let Some(l) = cx.tree.labels.get(&node.id) {
        let txt = clip(l, 24);
        let w = pill(p, ink, pill_right, cy, &txt, PillStyle::Soft(pal.warn));
        pill_right -= w + 6.0;
    }

    // Text: fill the card up to the leftmost pill (or the age / hover icons).
    let max_w = (pill_right - 8.0 - text_left).max(40.0);
    let (font, color, bold) = match row.class {
        RowClass::User => (ink.prose(13.5), ink.a, true),
        RowClass::Assistant => (ink.prose(13.0), pal.text, false),
        RowClass::Tool => (ink.mono_px(11.0), pal.weak, false),
        RowClass::Compaction => (ink.prose(12.5), pal.warn, false),
        RowClass::Summary => (ink.prose(12.5), ink.b, false),
        RowClass::Other => (ink.prose(12.5), pal.weak, false),
    };
    let color = if row.class == RowClass::Assistant && node.text.starts_with('(') {
        pal.weak
    } else if row.class == RowClass::User {
        ink.legible(color, ink.wash(ink.a), 3.0)
    } else {
        tone(color)
    };
    let shown = if node.text.is_empty() {
        tree_layout::kind_name(&node.kind)
    } else {
        &node.text
    };
    let g = gc.row((row.node, row.on_path, max_w.to_bits()), || {
        let spans = if cx.tokens.is_empty() {
            Vec::new()
        } else {
            tree_layout::highlight_spans(&node.text, cx.tokens)
        };
        p.layout_job(text_job(shown, font, color, bold, pal, &spans, max_w))
    });
    p.galley(pos2(text_left, cy - g.size().y / 2.0), g, color);

    // Age, or the hover actions in its place.
    if cx.show_actions {
        return actions(ui, p, ink, cx, u, card, right);
    }
    if let Some(g) = age {
        p.galley(pos2(right - g.size().x, cy - g.size().y / 2.0), g, pal.weak);
    }
    None
}

/// `$1.23`, or `≥$1.23` when some earlier usage could not be priced.
fn cost_amount(cost: RecordedCost) -> String {
    format!(
        "{}{}",
        if cost.incomplete { "≥" } else { "" },
        deck_service::statusfmt::fmt_cost(cost.total),
    )
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn text_job(
    text: &str,
    font: FontId,
    color: Color32,
    bold: bool,
    pal: &Palette,
    spans: &[std::ops::Range<usize>],
    max_w: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut base = TextFormat {
        font_id: font.clone(),
        color,
        background: Color32::TRANSPARENT,
        ..TextFormat::default()
    };
    if bold {
        base.coords.push("wght", 700.0);
    }
    let bold_fmt = base;
    let mut at = 0;
    for s in spans {
        if s.start > at {
            job.append(&text[at..s.start], 0.0, bold_fmt.clone());
        }
        let mut hl = TextFormat {
            font_id: font.clone(),
            color: pal.text_strong,
            background: pal.accent.gamma_multiply(0.38),
            ..TextFormat::default()
        };
        if bold {
            hl.coords.push("wght", 700.0);
        }
        job.append(&text[s.clone()], 0.0, hl);
        at = s.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, bold_fmt);
    }
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

#[derive(Clone, Copy)]
enum PillStyle {
    Solid,
    Soft(Color32),
}

/// Right-aligned pill ending at `right`; returns its width.
fn pill(p: &egui::Painter, ink: &Ink, right: f32, cy: f32, text: &str, style: PillStyle) -> f32 {
    let pal = ink.p;
    let fg = match style {
        PillStyle::Solid => theme::on_accent(pal),
        PillStyle::Soft(c) => {
            if theme::flat() {
                pal.text
            } else {
                ink.legible(c, ink.wash(c), 3.0)
            }
        }
    };
    let g = p.layout_no_wrap(text.to_owned(), ink.label(10.5), fg);
    let w = g.size().x + 14.0;
    let rect = Rect::from_min_size(pos2(right - w, cy - 9.0), vec2(w, 18.0));
    match style {
        PillStyle::Solid => theme::gradient_rounded(p, rect, 4.0, pal.accent_a, pal.accent_b),
        PillStyle::Soft(c) => {
            let fill = if theme::flat() {
                pal.raised
            } else {
                ink.wash(c)
            };
            p.rect_filled(rect, 4, fill);
            p.rect_stroke(
                rect,
                4,
                crate::badge::outline(p.pixels_per_point(), ink.edge(c)),
                StrokeKind::Inside,
            );
        }
    }
    p.galley(pos2(rect.left() + 7.0, cy - g.size().y / 2.0), g, fg);
    w
}

/// Label / re-answer / Fork / Go here buttons of the active row.
fn actions(
    ui: &mut Ui,
    p: &egui::Painter,
    ink: &Ink,
    cx: &ContentCtx<'_>,
    u: &RowUi,
    card: Rect,
    right: f32,
) -> Option<RowCommand> {
    let pal = ink.p;
    let row = &cx.layout.rows[u.row];
    let node = &cx.tree.nodes[row.node];
    let cy = card.center().y;
    let fork_off = fork_blocked(cx.tree, cx.layout, cx.input, u.row);
    let go_off = go_blocked(cx.layout, cx.input, u.row);
    let go_tip = if matches!(node.kind, NodeKind::User) {
        "Go to just before this prompt; its text goes back to the editor"
    } else {
        "Continue from here in this tab"
    };
    let label_tip = if cx.tree.labels.contains_key(&node.id) {
        "Edit this entry's label (L)"
    } else {
        "Add a label to this entry (L)"
    };
    let mut cmd = None;
    let h = card.height().min(26.0);
    let mut x = right;
    x -= action_button(
        ui,
        p,
        pal,
        (x, cy, h),
        ("", ICON_ARROW_RIGHT),
        (go_off, go_tip),
        u,
        "go",
        &mut cmd,
        RowCommand::Go,
    ) + 6.0;
    x -= action_button(
        ui,
        p,
        pal,
        (x, cy, h),
        ("", ICON_GIT_FORK),
        (fork_off, tree_layout::fork_hint(&node.kind)),
        u,
        "fork",
        &mut cmd,
        RowCommand::Fork,
    ) + 6.0;
    if tree_layout::is_ask_result(node) {
        x -= action_button(
            ui,
            p,
            pal,
            (x, cy, h),
            ("", ICON_MESSAGES_SQUARE),
            (
                reanswer_blocked(cx.tree, cx.layout, cx.input, u.row),
                "Re-answer: omp asks the question again in the tab and continues from your new answer",
            ),
            u,
            "reanswer",
            &mut cmd,
            RowCommand::Reanswer,
        ) + 6.0;
    }
    action_button(
        ui,
        p,
        pal,
        (x, cy, h),
        ("", ICON_BOOKMARK),
        (cx.input.label.err(), label_tip),
        u,
        "label",
        &mut cmd,
        RowCommand::Label,
    );
    cmd
}

#[allow(clippy::too_many_arguments)]
fn action_button(
    ui: &mut Ui,
    p: &egui::Painter,
    pal: &Palette,
    (right, cy, h): (f32, f32, f32),
    (label, icon): (&str, Icon),
    (off, tip): (Option<&str>, &str),
    u: &RowUi,
    id: &str,
    cmd: &mut Option<RowCommand>,
    on_click: RowCommand,
) -> f32 {
    let enabled = off.is_none();
    let _ = (p, label);
    let w = 32.0;
    let rect = Rect::from_min_size(pos2(right - w, cy - h / 2.0), vec2(w, h.min(26.0)));
    let resp = ui.interact(
        rect,
        Id::unique("tree-act").with((u.row, id)),
        // Sensed even when unavailable, so the button (not the row under it) owns the hover and
        // shows why; the click is ignored below.
        Sense::click(),
    );
    let hov = resp.hovered() && enabled;
    let fg = if enabled {
        theme::mix(pal.text, pal.text_strong, f32::from(u8::from(hov)))
    } else {
        pal.weak.gamma_multiply(0.55)
    };
    if enabled {
        p.rect_filled(
            rect,
            13,
            if hov {
                theme::accent_fill(pal, 0.3)
            } else {
                pal.surface
            },
        );
        p.rect_stroke(
            rect,
            13,
            Stroke::new(1.0, if hov { pal.accent } else { pal.stroke }),
            StrokeKind::Inside,
        );
    } else {
        // Unavailable: no fill, faint outline; the reason shows on hover.
        p.rect_stroke(
            rect,
            13,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.5)),
            StrokeKind::Inside,
        );
    }
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        if enabled { pal.accent } else { fg },
    );
    let resp = match off {
        Some(why) => resp.on_hover_text(why),
        None => resp
            .on_hover_text(tip)
            .on_hover_cursor(CursorIcon::PointingHand),
    };
    if resp.clicked() && enabled {
        *cmd = Some(on_click);
    }
    w
}

#[cfg(test)]
mod tests;
