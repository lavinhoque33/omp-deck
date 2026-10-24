//! The session tree window: a git-graph style picker over [`crate::tree_layout`]. Rows are
//! virtualized; the layout is rebuilt only when the tree revision, the leaf or the filter
//! changes.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::icons::Icon;
use crate::icons::{
    ICON_ALIGN_LEFT, ICON_ARROW_RIGHT, ICON_BOOKMARK, ICON_COPY, ICON_CORNER_DOWN_RIGHT,
    ICON_CROSSHAIR, ICON_FOLD_VERTICAL, ICON_GIT_FORK, ICON_KEY_ROUND, ICON_LIST_TREE,
    ICON_SCROLL_TEXT, ICON_SPARKLES, ICON_USER, ICON_WRENCH, ICON_X,
};
use deck_core::tree::{NodeKind, SessionTree};
use egui::Galley;
use egui::epaint::CubicBezierShape;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CursorIcon, FontId, Frame, Id, Key, Margin, Modifiers, Order, Painter,
    Pos2, Rect, RichText, Sense, Sides, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2,
};

use crate::theme::{self, Palette};
use crate::tree_layout::{self, Filter, Layout, Row, RowClass};
use crate::widgets::{empty_state, menu_item, paint_icon};

/// Uniform row height (virtualization relies on it).
pub const ROW_H: f32 = 38.0;
const LANE_W: f32 = 18.0;
const FULL_LANES: usize = 7;
const PAD_L: f32 = 18.0;
/// Right-hand column: the age, or the hover actions in its place.
const ACTIONS_W: f32 = 176.0;

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
    },
    Copy {
        text: String,
        what: &'static str,
    },
    /// A keyboard shortcut that cannot run right now.
    Notice(String),
}

pub struct TreeInput<'a> {
    pub tree: Option<&'a Arc<SessionTree>>,
    pub revision: u64,
    pub error: Option<&'a str>,
    /// The live leaf entry id, when the session has a control link.
    pub live_leaf: Option<&'a str>,
    /// `Ok` = Go here is available; `Err` = why it is not.
    pub navigate: Result<(), &'a str>,
    /// `Ok` = Fork is available; `Err` = why it is not.
    pub fork: Result<(), &'a str>,
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
    focus_search: bool,
    /// The search box had focus at the end of the previous frame: egui drops focus on Esc
    /// before the app sees the key, so this is what tells "Esc in the box" from "Esc".
    search_focused: bool,
    galleys: GalleyCache,
    search_id: Id,
    scratch: Vec<RowUi>,
    long: Vec<u32>,
}

impl TreeState {
    pub fn new(title: String) -> TreeState {
        TreeState {
            title,
            filter: Filter::default(),
            search: String::new(),
            cache: None,
            hits: None,
            selected: None,
            want: Want::Leaf,
            pinned: true,
            last_offset: 0.0,
            settle: 0,
            focus_search: false,
            search_focused: false,
            search_id: Id::unique("tree-search"),
            galleys: GalleyCache::default(),
            scratch: Vec::new(),
            long: Vec::new(),
        }
    }
}

/// What a row's text layout depends on besides the node and the width.
struct GalleyKey<'a> {
    revision: u64,
    ppp: u32,
    theme: [Color32; 6],
    tokens: &'a [String],
}

/// Laid-out row texts, so a frame does not rebuild a `LayoutJob` per visible row. Valid for one
/// (tree revision, pixel ratio, theme, search tokens); keyed per row by (node, on path, width).
#[derive(Default)]
struct GalleyCache {
    valid: bool,
    revision: u64,
    ppp: u32,
    theme: [Color32; 6],
    tokens: Vec<String>,
    map: HashMap<(usize, bool, u32), Arc<Galley>>,
}

/// More laid-out rows than a long scroll through a big tree needs.
const GALLEY_CAP: usize = 4096;

impl GalleyCache {
    fn sync(&mut self, k: GalleyKey<'_>) {
        if self.valid
            && self.revision == k.revision
            && self.ppp == k.ppp
            && self.theme == k.theme
            && self.tokens == k.tokens
        {
            return;
        }
        self.valid = true;
        self.revision = k.revision;
        self.ppp = k.ppp;
        self.theme = k.theme;
        self.tokens = k.tokens.to_vec();
        self.map.clear();
    }

    fn row(&mut self, key: (usize, bool, u32), build: impl FnOnce() -> Arc<Galley>) -> Arc<Galley> {
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
    rect: Rect,
    hovered: bool,
}

/// Lane spacing and gutter width for `lanes` lanes: wide trees compress instead of growing.
pub fn gutter(lanes: u16) -> (f32, f32) {
    let n = usize::from(lanes.max(1));
    let step = if n <= FULL_LANES {
        LANE_W
    } else {
        (FULL_LANES as f32 * LANE_W / n as f32).max(6.0)
    };
    (step, n as f32 * step)
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

// ------------------------------------------------------------------ colours

fn class_color(pal: &Palette, class: RowClass) -> Color32 {
    match class {
        RowClass::User if theme::flat() => pal.text_strong,
        RowClass::Assistant if theme::flat() => pal.text,
        RowClass::User => pal.accent_b,
        RowClass::Assistant => pal.accent,
        RowClass::Tool => pal.weak,
        RowClass::Compaction => pal.warn,
        RowClass::Summary => pal.ok,
        RowClass::Other => pal.weak,
    }
}

fn class_icon(class: RowClass) -> Icon {
    match class {
        RowClass::User => ICON_USER,
        RowClass::Assistant => ICON_SPARKLES,
        RowClass::Tool => ICON_WRENCH,
        RowClass::Compaction => ICON_FOLD_VERTICAL,
        RowClass::Summary => ICON_SCROLL_TEXT,
        RowClass::Other => ICON_ALIGN_LEFT,
    }
}

/// Muted tint of an inactive branch's line, by lane.
fn lane_color(pal: &Palette, lane: u16) -> Color32 {
    let hues = [pal.accent_b, pal.warn, pal.ok, pal.danger, pal.accent_a];
    theme::mix(pal.weak, hues[usize::from(lane) % hues.len()], 0.38).gamma_multiply(0.85)
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
            .clamp(620.0, 1500.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.82)
            .clamp(420.0, 1150.0)
            .min(screen.height() - 24.0),
    );
    let style = ctx.global_style();
    let frame = Frame::NONE
        .fill(pal.bar)
        .stroke(theme::popup_stroke(pal))
        .corner_radius(theme::popup_radius())
        .shadow(style.visuals.window_shadow);
    egui::Window::new("tree-window")
        .id(Id::unique("tree-window"))
        .title_bar(false)
        .frame(frame)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(560.0, 360.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| body(ui, st, input, &mut out));
    st.search_focused = ctx.memory(|m| m.has_focus(st.search_id));
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
        let first = st.cache.is_none();
        st.cache = Some(Cached {
            revision: input.revision,
            leaf,
            filter: st.filter,
            layout,
        });
        let Some(c) = &st.cache else {
            return;
        };
        let keep = st
            .selected
            .as_ref()
            .and_then(|id| tree.index.get(id.as_str()))
            .is_some_and(|&i| c.layout.row_of_node(i).is_some());
        if first || !keep {
            // omp opens `/tree` on the leaf.
            st.selected = c
                .layout
                .leaf_row
                .map(|r| tree.nodes[c.layout.rows[r].node].id.clone());
            if first {
                st.want = Want::Leaf;
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
        let tokens = tree_layout::search_tokens(&st.search);
        let rows = if tokens.is_empty() {
            Vec::new()
        } else {
            tree_layout::search_matches(tree, &c.layout, &tokens)
        };
        st.hits = Some(Hits {
            query: st.search.clone(),
            revision: c.revision,
            filter: c.filter,
            leaf: c.leaf,
            tokens,
            rows,
        });
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
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            out.push(TreeAction::Close);
        }
        return;
    };
    let searching = st.search_focused || ctx.memory(|m| m.has_focus(st.search_id));
    let menu_open = egui::Popup::is_any_open(ctx);
    let (esc, up, down, pgup, pgdn, home, end, enter, fork, slash) = ctx.input_mut(|i| {
        let n = Modifiers::NONE;
        (
            !menu_open && i.consume_key(n, Key::Escape),
            i.consume_key(n, Key::ArrowUp),
            i.consume_key(n, Key::ArrowDown),
            i.consume_key(n, Key::PageUp),
            i.consume_key(n, Key::PageDown),
            !searching && i.consume_key(n, Key::Home),
            !searching && i.consume_key(n, Key::End),
            !menu_open && i.consume_key(n, Key::Enter),
            !searching && !menu_open && i.consume_key(n, Key::F),
            !searching && !menu_open && i.consume_key(Modifiers::NONE, Key::Slash)
                || i.consume_key(Modifiers::COMMAND, Key::F),
        )
    });
    if esc {
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
    let Some(c) = &st.cache else {
        return;
    };
    let len = c.layout.rows.len();
    let cur = selected_row(st, tree);
    let delta = match (up, down, pgup, pgdn) {
        (true, ..) => Some(-1),
        (_, true, ..) => Some(1),
        (_, _, true, _) => Some(-12),
        (.., true) => Some(12),
        _ => None,
    };
    let target = if home {
        step_row(Some(0), 0, len)
    } else if end {
        len.checked_sub(1)
    } else {
        delta.and_then(|d| step_row(cur, d, len))
    };
    if let Some(r) = target
        && Some(r) != cur
    {
        select_row(st, tree, r);
        st.want = Want::Selected;
    }
    if enter && let Some(r) = selected_row(st, tree) {
        go_here(st, tree, input, r, out);
    }
    if fork && let Some(r) = selected_row(st, tree) {
        fork_here(st, tree, input, r, out);
    }
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
fn go_blocked<'a>(
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'a>,
    r: usize,
) -> Option<&'a str> {
    if let Err(why) = input.navigate {
        return Some(why);
    }
    let row = &layout.rows[r];
    (layout.leaf_node == Some(row.node) && !matches!(tree.nodes[row.node].kind, NodeKind::User))
        .then_some("This is where the session already is")
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

fn go_here(
    st: &TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    match go_blocked(tree, &c.layout, input, r) {
        Some(why) => out.push(TreeAction::Notice(why.to_owned())),
        None => {
            let n = &tree.nodes[c.layout.rows[r].node];
            out.push(TreeAction::Navigate {
                entry: n.id.clone(),
                user: matches!(n.kind, NodeKind::User),
            });
        }
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

// ------------------------------------------------------------------ layout of the window

fn body(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
    ui.spacing_mut().scroll = egui::style::ScrollStyle::floating();

    footer(ui, st, input);

    Frame::NONE
        .inner_margin(Margin {
            left: 22,
            right: 18,
            top: 16,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            header(ui, st, input, out);
        });
    let y = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().left()..=ui.max_rect().right(),
        y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
    );

    let Some(tree) = input.tree else {
        centered_status(ui, input);
        return;
    };
    let empty = st.cache.as_ref().is_none_or(|c| c.layout.rows.is_empty());
    if empty {
        let (title, sub) = match st.filter {
            Filter::Labeled => (
                "No labeled entries",
                "Labels you add with /label show up here.",
            ),
            _ => ("Nothing to show", "This session has no entries yet."),
        };
        empty_state(ui, ICON_LIST_TREE, pal.weak, title, sub);
        return;
    }
    list(ui, st, tree, input, out);
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

fn header(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            ui.painter().rect_filled(
                rect,
                theme::rad(8, 10),
                if theme::flat() {
                    pal.surface
                } else {
                    pal.accent.gamma_multiply(0.18)
                },
            );
            paint_icon(
                ui.painter(),
                rect.center(),
                Align2::CENTER_CENTER,
                ICON_LIST_TREE,
                19.0,
                pal.glyph,
            );
            ui.label(
                theme::strong("Session tree")
                    .size(19.0)
                    .color(pal.text_strong),
            );
            if !st.title.is_empty() {
                ui.add(
                    egui::Label::new(RichText::new(&st.title).color(pal.weak).size(14.0))
                        .truncate(),
                );
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let close = ui.add_sized(
                [30.0, 30.0],
                egui::Button::new(icon(ICON_X, 20.0, pal.weak)).frame(false),
            );
            if close.on_hover_text("Close (Esc)").clicked() {
                out.push(TreeAction::Close);
            }
        },
    );
    ui.add_space(4.0);
    let mut filter = st.filter;
    let focus = std::mem::take(&mut st.focus_search);
    let search_id = st.search_id;
    let search = &mut st.search;
    Sides::new().height(30.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for f in Filter::ALL {
                if chip(ui, f.title(), f.hint(), filter == f).clicked() {
                    filter = f;
                }
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            search_box(ui, search, search_id, focus);
        },
    );
    if filter != st.filter {
        st.filter = filter;
        st.want = Want::Selected;
    }
    ui.add_space(2.0);
    if let Some(c) = &st.cache {
        let s = c.layout.stats;
        let plural = |n: usize, one: &str, many: &str| {
            format!("{} {}", fmt_count(n), if n == 1 { one } else { many })
        };
        let mut line = format!(
            "{} of {}",
            fmt_count(s.shown),
            plural(s.entries, "entry", "entries")
        );
        if s.tips > 1 {
            line.push_str(" · ");
            line.push_str(&plural(s.tips, "branch", "branches"));
        }
        if s.forks > 0 {
            line.push_str(" · ");
            line.push_str(&plural(s.forks, "fork point", "fork points"));
        }
        if input.live_leaf.is_some() {
            line.push_str(" · live");
        }
        ui.label(RichText::new(line).size(12.0).color(pal.weak));
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

/// Filter pill.
fn chip(ui: &mut Ui, label: &str, tip: &str, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), crate::theme::font(13.0), pal.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 26.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let r = theme::rad(8, 14);
    if on {
        let fill = if theme::flat() {
            pal.raised
        } else {
            pal.accent.gamma_multiply(0.22)
        };
        p.rect_filled(rect, r, fill);
        p.rect_stroke(
            rect,
            r,
            Stroke::new(
                1.0,
                pal.accent
                    .gamma_multiply(if theme::flat() { 0.8 } else { 0.7 }),
            ),
            StrokeKind::Inside,
        );
    } else {
        p.rect_filled(rect, r, pal.raised.gamma_multiply(0.7 * hov));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            StrokeKind::Inside,
        );
    }
    let color = if on {
        pal.text_strong
    } else {
        theme::mix(pal.weak, pal.text_strong, hov)
    };
    p.galley(rect.center() - g.size() / 2.0, g, color);
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

fn search_box(ui: &mut Ui, search: &mut String, id: Id, focus: bool) {
    let pal = Palette::of_ui(ui);
    if !search.is_empty() {
        let clear = ui.add(egui::Button::new(icon(ICON_X, 16.0, pal.weak)).frame(false));
        if clear.on_hover_text("Clear search").clicked() {
            search.clear();
        }
    }
    let edit = TextEdit::singleline(search)
        .id(id)
        .hint_text("Search prompts, replies, labels…  (/)")
        .desired_width(270.0)
        .margin(vec2(10.0, 6.0));
    let r = ui.add(edit);
    if focus {
        r.request_focus();
    }
}

fn footer(ui: &mut Ui, st: &TreeState, input: &TreeInput<'_>) {
    let pal = Palette::of_ui(ui);
    egui::Panel::bottom("tree-footer")
        .show_separator_line(false)
        .exact_size(40.0)
        .frame(Frame::NONE.inner_margin(Margin::symmetric(22, 8)))
        .show(ui, |ui| {
            let y = ui.max_rect().top();
            ui.painter().hline(
                ui.max_rect().left() - 22.0..=ui.max_rect().right() + 22.0,
                y - 8.0,
                Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            );
            Sides::new().height(24.0).show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    for (key, what) in [
                        ("↑ ↓", "select"),
                        ("Enter", "go here"),
                        ("F", "fork"),
                        ("/", "search"),
                        ("Esc", "close"),
                    ] {
                        keycap(ui, key, what);
                    }
                },
                |ui| {
                    let (text, color) = match (&input.navigate, st.cache.is_some()) {
                        (Err(why), true) => (format!("Go here is off: {why}"), pal.weak),
                        (Ok(()), true) => (
                            "Go here re-renders the tab at the chosen point".to_owned(),
                            pal.weak,
                        ),
                        _ => (String::new(), pal.weak),
                    };
                    ui.add(
                        egui::Label::new(RichText::new(text).size(12.0).color(color)).truncate(),
                    );
                },
            );
        });
}

fn keycap(ui: &mut Ui, key: &str, what: &str) {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(key.to_owned(), crate::theme::font(11.5), pal.text);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 20.0), Sense::hover());
    ui.painter().rect_filled(rect, 6, pal.surface);
    ui.painter()
        .rect_stroke(rect, 6, Stroke::new(1.0, pal.stroke), StrokeKind::Inside);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, pal.text);
    ui.label(RichText::new(what).size(12.0).color(pal.weak));
}

// ------------------------------------------------------------------ the list

struct Geometry {
    left: f32,
    top: f32,
    step: f32,
    graph_w: f32,
}

impl Geometry {
    fn x(&self, lane: u16) -> f32 {
        self.left + PAD_L + (f32::from(lane) + 0.5) * self.step
    }
    fn y(&self, row: usize) -> f32 {
        self.top + (row as f32 + 0.5) * ROW_H
    }
    fn at(&self, row: usize, lane: u16) -> Pos2 {
        pos2(self.x(lane), self.y(row))
    }
    fn text_x(&self) -> f32 {
        self.left + PAD_L + self.graph_w + 10.0
    }
}

fn list(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &Arc<SessionTree>,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    let Some(layout) = st.cache.as_ref().map(|c| &c.layout) else {
        return;
    };
    let rows = layout.rows.len();
    let (step, graph_w) = gutter(layout.lanes);
    let total_h = rows as f32 * ROW_H + 12.0;
    let want = std::mem::replace(&mut st.want, Want::None);
    let sel = selected_row(st, tree);
    let leaf_row = layout.leaf_row;
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
        tokens: dim_hits.map_or(&[], |h| &h.tokens[..]),
    });

    let mut clicked: Option<usize> = None;
    let mut actions: Vec<(usize, RowCommand)> = Vec::new();
    let area = egui::ScrollArea::vertical()
        .id_salt("tree-scroll")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), total_h), Sense::hover());
            let geo = Geometry {
                left: rect.left(),
                top: rect.top(),
                step,
                graph_w,
            };
            if let Some(r) = target {
                let row = Rect::from_min_size(
                    pos2(rect.left(), geo.top + r as f32 * ROW_H),
                    vec2(rect.width(), ROW_H),
                );
                let align = if want == Want::Leaf {
                    Some(Align::Center)
                } else {
                    None
                };
                ui.scroll_to_rect_animation(
                    row.expand2(vec2(0.0, ROW_H)),
                    align,
                    egui::style::ScrollAnimation::none(),
                );
            }
            let first = ((viewport.min.y / ROW_H).floor().max(0.0) as usize).min(rows);
            let last = ((viewport.max.y / ROW_H).ceil().max(0.0) as usize + 1).min(rows);
            let hover_pos = ui.input(|i| i.pointer.hover_pos());
            st.scratch.clear();
            for r in first..last {
                let rr = Rect::from_min_size(
                    pos2(rect.left(), geo.top + r as f32 * ROW_H),
                    vec2(rect.width(), ROW_H),
                );
                let resp = ui.interact(rr, Id::unique("tree-row").with(r), Sense::click());
                let hovered = hover_pos
                    .is_some_and(|p| rr.contains(p) && ui.clip_rect().contains(p))
                    && !egui::Popup::is_any_open(ui.ctx());
                if resp.clicked() {
                    clicked = Some(r);
                }
                if let Some(c) = &st.cache {
                    let node = c.layout.rows[r].node;
                    row_menu(&resp, tree, &c.layout, input, r, node, &mut actions);
                    // The action buttons carry their own tooltip.
                    let over_actions =
                        hover_pos.is_some_and(|p| p.x >= rr.right() - 18.0 - ACTIONS_W);
                    if hovered && !over_actions && !resp.context_menu_opened() {
                        resp.on_hover_ui(|ui| row_tip(ui, tree, &c.layout.rows[r], input.now));
                    }
                }
                st.scratch.push(RowUi {
                    row: r,
                    rect: rr,
                    hovered,
                });
            }
            let Some(layout) = st.cache.as_ref().map(|c| &c.layout) else {
                return;
            };
            // Backgrounds, then the graph, then row content on top.
            let painter = ui.painter().clone();
            let hover_row = st.scratch.iter().find(|u| u.hovered).map(|u| u.row);
            for u in &st.scratch {
                paint_band(
                    &painter,
                    pal,
                    layout,
                    u,
                    sel == Some(u.row),
                    hover_row == Some(u.row),
                );
            }
            paint_graph(&painter, pal, layout, &geo, first, last, &mut st.long);
            let active = hover_row.or(sel);
            for u in &st.scratch {
                let row = &layout.rows[u.row];
                let dim = match dim_hits {
                    Some(h) if !h.rows.get(u.row).copied().unwrap_or(false) => 0.32,
                    _ => 1.0,
                };
                paint_node(
                    &painter,
                    pal,
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
                    pal,
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
                go_here(st, tree, input, r, out);
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
    let Some(leaf_row) = st.cache.as_ref().and_then(|c| c.layout.leaf_row) else {
        return;
    };
    let top = st.last_offset;
    let y = leaf_row as f32 * ROW_H;
    if y + ROW_H > top && y < top + list.height() {
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
        let go_off = go_blocked(tree, layout, input, r);
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

fn row_tip(ui: &mut Ui, tree: &SessionTree, row: &Row, now: i64) {
    let pal = Palette::of_ui(ui);
    let n = &tree.nodes[row.node];
    ui.set_max_width(460.0);
    ui.label(
        RichText::new(tree_layout::kind_name(&n.kind))
            .small()
            .color(class_color(pal, row.class)),
    );
    if !n.text.is_empty() {
        ui.label(&n.text);
    }
    if let Some(l) = tree.labels.get(&n.id) {
        ui.label(RichText::new(format!("label: {l}")).color(pal.warn));
    }
    if let Some(ts) = row.ts {
        ui.weak(format!(
            "{} · {} ago",
            tree_layout::clock(ts),
            tree_layout::age(now, ts)
        ));
    }
    ui.weak(format!("id {}", n.id));
}

// ------------------------------------------------------------------ painting

fn paint_band(
    p: &Painter,
    pal: &Palette,
    layout: &Layout,
    u: &RowUi,
    selected: bool,
    hovered: bool,
) {
    let row = &layout.rows[u.row];
    let inner = u.rect.shrink2(vec2(8.0, 2.0));
    let r = theme::rad(8, 10);
    if row.is_leaf {
        let fill = if theme::flat() {
            pal.surface
        } else {
            pal.accent.gamma_multiply(0.11)
        };
        p.rect_filled(inner, r, fill);
    }
    if hovered {
        p.rect_filled(inner, r, pal.raised.gamma_multiply(0.55));
    }
    if selected {
        let fill = if theme::flat() {
            pal.raised
        } else {
            pal.accent.gamma_multiply(0.16)
        };
        p.rect_filled(inner, r, fill);
        p.rect_stroke(
            inner,
            r,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.55)),
            StrokeKind::Inside,
        );
    }
}

/// Edges of the rows in `first..last`, plus the long edges crossing that range. Inactive
/// branches first so the active path draws on top.
fn paint_graph(
    p: &Painter,
    pal: &Palette,
    layout: &Layout,
    geo: &Geometry,
    first: usize,
    last: usize,
    scratch: &mut Vec<u32>,
) {
    scratch.clear();
    let from = first.saturating_sub(1);
    scratch.extend((from..last).map(|r| r as u32));
    // Long edges whose child row lies outside the window but whose span crosses it.
    for &r in &layout.long_edges {
        let r_us = r as usize;
        if (from..last).contains(&r_us) {
            continue;
        }
        if let Some(parent) = layout.rows[r_us].parent
            && (parent as usize) < last
            && r_us >= from
        {
            scratch.push(r);
        }
    }
    for pass in [false, true] {
        for &r in scratch.iter() {
            let row = &layout.rows[r as usize];
            if row.on_path != pass {
                continue;
            }
            let Some(parent) = row.parent else {
                continue;
            };
            let prow = &layout.rows[parent as usize];
            let (a, b) = (
                geo.at(parent as usize, prow.lane),
                geo.at(r as usize, row.lane),
            );
            let stroke = if row.on_path {
                Stroke::new(2.6, pal.accent)
            } else {
                Stroke::new(1.6, lane_color(pal, row.lane))
            };
            edge(p, a, b, stroke);
        }
    }
}

/// Parent `a` → child `b`: straight in a lane, or a smooth S-curve into the child's lane over the
/// first row, then straight down to the child.
fn edge(p: &Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    if (a.x - b.x).abs() < 0.5 {
        p.line_segment([a, b], stroke);
        return;
    }
    let end = pos2(b.x, (a.y + ROW_H).min(b.y));
    let h = end.y - a.y;
    p.add(CubicBezierShape::from_points_stroke(
        [
            a,
            pos2(a.x, a.y + h * 0.62),
            pos2(b.x, end.y - h * 0.62),
            end,
        ],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
    if b.y > end.y {
        p.line_segment([end, b], stroke);
    }
}

fn paint_node(p: &Painter, pal: &Palette, row: &Row, c: Pos2, fork_point: bool) {
    let base = pal.bar;
    if row.is_leaf && !theme::flat() {
        p.circle_filled(c, 15.0, pal.accent.gamma_multiply(0.14));
        p.circle_filled(c, 10.5, pal.accent.gamma_multiply(0.26));
    }
    if fork_point {
        p.circle_stroke(c, 8.5, Stroke::new(1.4, pal.accent_b.gamma_multiply(0.8)));
    }
    if row.on_path {
        p.circle_filled(c, 6.2, base);
        p.circle_filled(c, 4.6, pal.accent);
        if row.is_leaf {
            p.circle_filled(c, 2.2, Color32::WHITE);
        }
    } else {
        let col = lane_color(pal, row.lane);
        p.circle_filled(c, 6.0, base);
        p.circle_filled(c, 4.4, col);
        p.circle_filled(c, 2.1, base);
    }
}

struct ContentCtx<'a> {
    tree: &'a SessionTree,
    layout: &'a Layout,
    input: &'a TreeInput<'a>,
    geo: &'a Geometry,
    tokens: &'a [String],
    show_actions: bool,
}

/// Glyph, text, pills, age or action buttons of one row.
fn paint_content(
    ui: &mut Ui,
    p: &Painter,
    pal: &Palette,
    cx: &ContentCtx<'_>,
    u: &RowUi,
    gc: &mut GalleyCache,
) -> Option<RowCommand> {
    let row = &cx.layout.rows[u.row];
    let node = &cx.tree.nodes[row.node];
    let cy = u.rect.center().y;
    let off_path = !row.on_path;
    let class_col = class_color(pal, row.class);
    let tone = |c: Color32| {
        if off_path {
            theme::mix(c, pal.weak, 0.3)
        } else {
            c
        }
    };

    // Role glyph.
    let gx = cx.geo.text_x() + 11.0;
    p.circle_filled(
        pos2(gx, cy),
        11.0,
        class_col.gamma_multiply(if off_path { 0.12 } else { 0.2 }),
    );
    paint_icon(
        p,
        pos2(gx, cy),
        Align2::CENTER_CENTER,
        class_icon(row.class),
        14.0,
        tone(class_col),
    );
    let text_left = gx + 11.0 + 10.0;

    // Right column and pills.
    let right = u.rect.right() - 18.0;
    let actions_left = right - ACTIONS_W;
    let mut pill_right = actions_left - 6.0;
    let mut pills_w = 0.0;
    if row.is_leaf {
        let w = pill(
            p,
            pal,
            pill_right,
            cy,
            "you are here",
            Some(ICON_CROSSHAIR),
            PillStyle::Solid,
        );
        pill_right -= w + 6.0;
        pills_w += w + 6.0;
    }
    if row.children > 1 {
        let txt = format!("{} branches", row.children);
        let w = pill(
            p,
            pal,
            pill_right,
            cy,
            &txt,
            Some(ICON_LIST_TREE),
            PillStyle::Soft(if theme::flat() {
                pal.text
            } else {
                pal.accent_b
            }),
        );
        pill_right -= w + 6.0;
        pills_w += w + 6.0;
    }
    if let Some(l) = cx.tree.labels.get(&node.id) {
        let txt = clip(l, 24);
        let w = pill(
            p,
            pal,
            pill_right,
            cy,
            &txt,
            Some(ICON_BOOKMARK),
            PillStyle::Soft(pal.warn),
        );
        pills_w += w + 6.0;
    }

    // Text.
    let max_w = (actions_left - 8.0 - pills_w - text_left).max(40.0);
    let (font, color) = match row.class {
        RowClass::User => (crate::theme::font(14.0), pal.text_strong),
        RowClass::Assistant => (crate::theme::font(13.5), pal.text),
        RowClass::Tool => (FontId::monospace(12.0), pal.weak),
        RowClass::Compaction => (crate::theme::font(13.0), pal.warn),
        RowClass::Summary => (crate::theme::font(13.0), pal.ok),
        RowClass::Other => (crate::theme::font(13.0), pal.weak),
    };
    let color = if row.class == RowClass::Assistant && node.text.starts_with('(') {
        pal.weak
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
        p.layout_job(text_job(shown, font, color, pal, &spans, max_w))
    });
    p.galley(pos2(text_left, cy - g.size().y / 2.0), g, color);

    // Age, or the hover actions in its place.
    if cx.show_actions {
        return actions(ui, p, pal, cx, u, right);
    }
    if let Some(ts) = row.ts {
        let age = tree_layout::age(cx.input.now, ts);
        p.text(
            pos2(right, cy),
            Align2::RIGHT_CENTER,
            age,
            crate::theme::font(12.0),
            pal.weak,
        );
    }
    None
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
    pal: &Palette,
    spans: &[Range<usize>],
    max_w: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let fmt = |c: Color32, bg: Color32| TextFormat {
        font_id: font.clone(),
        color: c,
        background: bg,
        ..TextFormat::default()
    };
    let mut at = 0;
    for s in spans {
        if s.start > at {
            job.append(&text[at..s.start], 0.0, fmt(color, Color32::TRANSPARENT));
        }
        job.append(
            &text[s.clone()],
            0.0,
            fmt(pal.text_strong, pal.accent.gamma_multiply(0.38)),
        );
        at = s.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, fmt(color, Color32::TRANSPARENT));
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
fn pill(
    p: &Painter,
    pal: &Palette,
    right: f32,
    cy: f32,
    text: &str,
    icon: Option<Icon>,
    style: PillStyle,
) -> f32 {
    let fg = match style {
        PillStyle::Solid => theme::on_accent(pal),
        PillStyle::Soft(c) => c,
    };
    let g = p.layout_no_wrap(text.to_owned(), crate::theme::font(11.5), fg);
    let icon_w = if icon.is_some() { 16.0 } else { 0.0 };
    let w = g.size().x + 20.0 + icon_w;
    let rect = Rect::from_min_size(pos2(right - w, cy - 10.0), vec2(w, 20.0));
    match style {
        PillStyle::Solid => theme::gradient_rounded(p, rect, 10.0, pal.accent_a, pal.accent_b),
        PillStyle::Soft(c) => {
            p.rect_filled(rect, 10, c.gamma_multiply(0.15));
            p.rect_stroke(
                rect,
                10,
                Stroke::new(1.0, c.gamma_multiply(0.45)),
                StrokeKind::Inside,
            );
        }
    }
    let mut x = rect.left() + 9.0;
    if let Some(i) = icon {
        paint_icon(p, pos2(x + 6.0, cy), Align2::CENTER_CENTER, i, 13.0, fg);
        x += 16.0;
    }
    p.galley(pos2(x, cy - g.size().y / 2.0), g, fg);
    w
}

/// Fork / Go here buttons of the active row.
fn actions(
    ui: &mut Ui,
    p: &Painter,
    pal: &Palette,
    cx: &ContentCtx<'_>,
    u: &RowUi,
    right: f32,
) -> Option<RowCommand> {
    let row = &cx.layout.rows[u.row];
    let node = &cx.tree.nodes[row.node];
    let cy = u.rect.center().y;
    let fork_off = fork_blocked(cx.tree, cx.layout, cx.input, u.row);
    let go_off = go_blocked(cx.tree, cx.layout, cx.input, u.row);
    let go_tip = if matches!(node.kind, NodeKind::User) {
        "Go to just before this prompt; its text goes back to the editor"
    } else {
        "Continue from here in this tab"
    };
    let mut cmd = None;
    let go_w = action_button(
        ui,
        p,
        pal,
        (right, cy),
        ("Go here", ICON_ARROW_RIGHT),
        (go_off, go_tip),
        u,
        "go",
        &mut cmd,
        RowCommand::Go,
    );
    action_button(
        ui,
        p,
        pal,
        (right - go_w - 6.0, cy),
        ("Fork", ICON_GIT_FORK),
        (fork_off, tree_layout::fork_hint(&node.kind)),
        u,
        "fork",
        &mut cmd,
        RowCommand::Fork,
    );
    cmd
}

#[allow(clippy::too_many_arguments)]
fn action_button(
    ui: &mut Ui,
    p: &Painter,
    pal: &Palette,
    (right, cy): (f32, f32),
    (label, icon): (&str, Icon),
    (off, tip): (Option<&str>, &str),
    u: &RowUi,
    id: &str,
    cmd: &mut Option<RowCommand>,
    on_click: RowCommand,
) -> f32 {
    let enabled = off.is_none();
    let g = p.layout_no_wrap(label.to_owned(), crate::theme::font(12.5), pal.text_strong);
    let w = g.size().x + 40.0;
    let rect = Rect::from_min_size(pos2(right - w, cy - 13.0), vec2(w, 26.0));
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
        pos2(rect.left() + 15.0, cy),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        if enabled { pal.accent } else { fg },
    );
    p.galley(pos2(rect.left() + 28.0, cy - g.size().y / 2.0), g, fg);
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
mod tests {
    use super::*;

    #[test]
    fn gutter_compresses_wide_trees_only() {
        assert_eq!(gutter(1), (LANE_W, LANE_W));
        assert_eq!(gutter(7), (LANE_W, 7.0 * LANE_W));
        let (step, width) = gutter(14);
        assert!(step < LANE_W && (width - 7.0 * LANE_W).abs() < 0.01);
        assert!(gutter(400).0 >= 6.0);
    }

    #[test]
    fn selection_steps_clamp_at_both_ends() {
        assert_eq!(step_row(Some(3), 1, 5), Some(4));
        assert_eq!(step_row(Some(4), 1, 5), Some(4));
        assert_eq!(step_row(Some(1), -12, 5), Some(0));
        assert_eq!(step_row(Some(1), 12, 5), Some(4));
        assert_eq!(step_row(None, 1, 5), Some(0));
        assert_eq!(step_row(None, -1, 5), Some(4));
        assert_eq!(step_row(Some(0), 1, 0), None);
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(22_691), "22,691");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn long_labels_are_clipped_with_an_ellipsis() {
        assert_eq!(clip("short", 24), "short");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }
}
