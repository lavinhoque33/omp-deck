//! The preview pane: the selected entry's content, omp's `preview:"side"`. A head band
//! names the kind, a meta row shows time / cost / model / entry id, the reading area renders the
//! entry's [`EntryDetail`] chat-style through the subagent popup's [`TranscriptView`], and an
//! action bar offers Go here / Summarize & go / Fork / Label / Re-answer / Copy. The label editor
//! lives here too (omp edits labels in the preview).
//!
//! [`read_entry`] blocks on file I/O, so it runs on a worker thread: one request in flight, the
//! newest wins, answers arrive over a channel, and the last results are cached by node id.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use deck_core::agents::TranscriptItem;
use deck_core::tree::{EntryDetail, NodeKind, SessionTree};
use egui::{
    Align, Color32, CursorIcon, Frame, Margin, Pos2, Rect, RichText, Sense, Sides, Stroke, Ui,
    pos2, vec2,
};

use crate::card;
use crate::chat_view::Ink;
use crate::chat_view::paint as chat_paint;
use crate::icons::{
    ICON_BOOKMARK, ICON_COINS, ICON_COPY, ICON_CROSSHAIR, ICON_FOLD_VERTICAL, ICON_GIT_FORK,
    ICON_LIST_TREE, ICON_MESSAGES_SQUARE, ICON_SCROLL_TEXT, ICON_SPARKLES, ICON_USER, ICON_WRENCH,
};
use crate::theme::{self, Palette};
use crate::transcript_view::TranscriptView;
use crate::tree_layout::{self};
use crate::tree_view::{
    GoPlan, TreeAction, TreeInput, TreeState, clip, cost_amount, finish_label, fork_blocked,
    go_blocked, go_plan, label_here, reanswer_blocked, selected_row,
};
use crate::widgets::{DraftButton, DraftKind};

/// Cached preview answers (by node id).
const CACHE_CAP: usize = 32;

/// What the preview pane shows for the selected row.
pub struct PreviewState {
    /// Node id + tree revision the pane shows (or is loading).
    shown: Option<(String, u64)>,
    /// Cached answers by node id. The session file is append-only and an entry's line never
    /// changes (labels live in the tree), so a cached entry stays good across tree revisions;
    /// only `InvalidData` invalidates it.
    cache: HashMap<String, PreviewData>,
    /// Insertion order of `cache` (for the cap).
    order: VecDeque<String>,
    /// The request on the worker, if any.
    inflight: Option<PreviewRequest>,
    /// `InvalidData` failures by node id: the tree revision the failed read ran at, and how many
    /// consecutive failures. A retry waits for a newer tree revision; a second failure shows an
    /// inline note instead of spinning.
    invalid: HashMap<String, (u64, u8)>,
    rx: Option<Receiver<PreviewAnswer>>,
    tx: Option<Sender<PreviewAnswer>>,
    /// The chat-style reading view.
    view: TranscriptView,
    /// Node id the reading view is synced to (`TranscriptView::sync` runs only on change).
    synced: Option<String>,
    /// Height of the label editor + action bar block, measured last frame.
    bottom_h: f32,
}

impl Default for PreviewState {
    fn default() -> PreviewState {
        PreviewState {
            shown: None,
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: None,
            invalid: HashMap::new(),
            rx: None,
            tx: None,
            view: TranscriptView::new(false).opened(),
            synced: None,
            bottom_h: 64.0,
        }
    }
}

struct PreviewData {
    /// Shared with the reading view: stored once when the answer lands, never cloned per frame.
    items: std::sync::Arc<Vec<TranscriptItem>>,
    truncated: bool,
    /// The row text fallback (non-message kinds with empty items show the row text).
    row_text: String,
    /// Item text the Copy button copies, built once with the answer.
    copy_text: String,
    /// Error text when the entry could not be read.
    error: Option<String>,
}

/// One worker request: read node `node` of the session file at tree revision `revision`.
#[derive(Clone)]
struct PreviewRequest {
    id: String,
    revision: u64,
}

struct PreviewAnswer {
    id: String,
    revision: u64,
    result: std::io::Result<EntryDetail>,
}

impl PreviewState {
    /// The cached copy text of one entry, if it has been read.
    pub fn cached_copy(&self, id: &str) -> Option<&str> {
        self.cache.get(id).map(|d| d.copy_text.as_str())
    }
}

/// The pane's selection key: (node id, tree revision, row text for the fallback).
fn selection_key(
    st: &TreeState,
    tree: &SessionTree,
    revision: u64,
) -> Option<(usize, String, u64, String)> {
    let c = st.cache.as_ref()?;
    let r = selected_row(st, tree)?;
    let node = c.layout.rows.get(r)?.node;
    let n = tree.nodes.get(node)?;
    Some((node, n.id.clone(), revision, n.text.clone()))
}

/// Poll the worker's answers (called once per frame, after the window): applies the newest
/// answer per id, caches by node id, and requests the selection when nothing covers it. Never
/// retries on the same frame: `InvalidData` waits for a newer tree revision, and a second
/// failure shows an inline note.
pub fn poll(st: &mut TreeState, input: &TreeInput<'_>, ctx: &egui::Context) {
    let Some(tree) = input.tree else {
        return;
    };
    // Request the selection when it changed (never per frame otherwise).
    let key = selection_key(st, tree, input.revision);
    let pv = &mut st.preview;
    // Drain the channel; the newest answer for each id wins.
    let mut landed: Vec<PreviewAnswer> = Vec::new();
    if let Some(rx) = &pv.rx {
        loop {
            match rx.try_recv() {
                Ok(a) => landed.push(a),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    pv.rx = None;
                    pv.tx = None;
                    pv.inflight = None;
                    break;
                }
            }
        }
    }
    // Keep only the newest answer per id (the channel preserves send order).
    landed.reverse();
    let mut seen = std::collections::HashSet::new();
    landed.retain(|a| seen.insert(a.id.clone()));
    for a in landed {
        let fresh = pv
            .inflight
            .as_ref()
            .is_some_and(|f| f.id == a.id && f.revision == a.revision);
        if pv
            .inflight
            .as_ref()
            .is_some_and(|f| f.id == a.id && f.revision == a.revision)
        {
            pv.inflight = None;
        }
        match a.result {
            Ok(detail) => {
                let row_text = row_text_of(tree, &a.id);
                let copy_text = copy_text_of(&detail.items, &row_text);
                let data = PreviewData {
                    items: std::sync::Arc::new(detail.items),
                    truncated: detail.truncated,
                    row_text,
                    copy_text,
                    error: None,
                };
                insert_cached(pv, a.id.clone(), data);
                pv.invalid.remove(&a.id);
                if fresh {
                    ctx.request_repaint();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // The file changed under the read: drop any stale entry. The retry below waits
                // for a newer tree revision than this failure's; a second failure shows a note.
                pv.cache.remove(&a.id);
                drop_cached(pv, &a.id);
                let entry = pv.invalid.entry(a.id.clone()).or_insert((a.revision, 0));
                entry.0 = a.revision;
                entry.1 = entry.1.saturating_add(1);
                if fresh {
                    ctx.request_repaint();
                }
            }
            Err(e) => {
                let row_text = row_text_of(tree, &a.id);
                let data = PreviewData {
                    items: std::sync::Arc::new(Vec::new()),
                    truncated: false,
                    row_text,
                    copy_text: String::new(),
                    error: Some(e.to_string()),
                };
                insert_cached(pv, a.id.clone(), data);
                pv.invalid.remove(&a.id);
                if fresh {
                    ctx.request_repaint();
                }
            }
        }
    }
    let Some((node, id, revision, _)) = key else {
        pv.shown = None;
        return;
    };
    pv.shown = Some((id.clone(), revision));
    if pv.cache.contains_key(&id) {
        return;
    }
    if pv.inflight.as_ref().is_some_and(|f| f.id == id) {
        return;
    }
    // No entry line (a gateway older than the GUI sends trees without `line`): never request;
    // the reading area shows the row text with an upgrade note.
    if tree.nodes.get(node).is_none_or(|n| n.line.is_none()) {
        return;
    }
    // After `InvalidData`, wait for a tree revision newer than the failure's; twice failed
    // stays a note. The count survives the retry (only a good answer clears it).
    if let Some((failed_at, n)) = pv.invalid.get(&id)
        && (*n >= 2 || revision <= *failed_at)
    {
        return;
    }
    request(pv, ctx, input, node, id, revision);
}

fn row_text_of(tree: &SessionTree, id: &str) -> String {
    tree.index
        .get(id)
        .and_then(|&i| tree.nodes.get(i))
        .map(|n| n.text.clone())
        .unwrap_or_default()
}

fn insert_cached(pv: &mut PreviewState, id: String, data: PreviewData) {
    if !pv.cache.contains_key(&id) {
        pv.order.push_back(id.clone());
        while pv.order.len() > CACHE_CAP {
            if let Some(old) = pv.order.pop_front() {
                pv.cache.remove(&old);
            }
        }
    }
    pv.synced = None;
    pv.cache.insert(id, data);
}

/// Drop one cached id from the order queue too (for invalidation).
fn drop_cached(pv: &mut PreviewState, id: &str) {
    if pv.synced.as_deref() == Some(id) {
        pv.synced = None;
    }
    pv.order.retain(|o| o != id);
}

/// Spawn the worker read of one entry: newest request wins (an older answer is ignored).
fn request(
    pv: &mut PreviewState,
    ctx: &egui::Context,
    input: &TreeInput<'_>,
    node: usize,
    id: String,
    revision: u64,
) {
    let (Some(session), Some(tree)) = (input.session, input.tree) else {
        return;
    };
    if pv.tx.is_none() {
        let (tx, rx) = std::sync::mpsc::channel();
        pv.tx = Some(tx);
        pv.rx = Some(rx);
    }
    let tx = pv.tx.clone();
    let path: PathBuf = session.to_owned();
    let tree: SessionTree = (**tree).clone();
    let ctx = ctx.clone();
    pv.inflight = Some(PreviewRequest {
        id: id.clone(),
        revision,
    });
    let spawned = std::thread::Builder::new()
        .name("deck-tree-preview".into())
        .spawn(move || {
            let result = deck_core::tree::read_entry(&path, &tree, node);
            if let Some(tx) = tx {
                let _ = tx.send(PreviewAnswer {
                    id,
                    revision,
                    result,
                });
            }
            ctx.request_repaint();
        });
    if spawned.is_err() {
        pv.inflight = None;
    }
}

// ------------------------------------------------------------------ the pane

pub fn pane(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let ink = Ink::new(ui, 12.0);
    let Some(r) = selected_row(st, tree) else {
        empty_pane(
            ui,
            &ink,
            "Nothing selected",
            "Pick a row to preview its entry.",
        );
        return;
    };
    let Some(node_idx) = st
        .cache
        .as_ref()
        .and_then(|c| c.layout.rows.get(r).map(|row| row.node))
    else {
        return;
    };
    let Some(node) = tree.nodes.get(node_idx) else {
        return;
    };
    let label = tree.labels.get(&node.id).cloned();
    let node_id = node.id.clone();
    pane_head(ui, &ink, node, label.as_deref());
    if meta_row(ui, &ink, st, tree, input, node_idx) {
        out.push(TreeAction::Copy {
            text: node_id.clone(),
            what: "entry id",
        });
    }
    // The editor + action bar sit at the pane's bottom edge; the reading area takes what is left.
    // The bar wraps on narrow panes, so its height is the one measured last frame (a change
    // repaints once).
    let full = ui.max_rect();
    let top = ui.cursor().top();
    let bottom_h = st
        .preview
        .bottom_h
        .clamp(0.0, (full.bottom() - top).max(0.0));
    reading(
        ui,
        st,
        tree,
        input,
        node_idx,
        (full.bottom() - top - bottom_h).max(60.0),
        out,
    );
    let mut bottom = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(full.left(), (full.bottom() - bottom_h).max(top)),
                full.max,
            ))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    if st.label.as_ref().is_some_and(|e| e.entry == node_id) {
        label_editor(&mut bottom, st, input, out);
    }
    action_bar(&mut bottom, st, tree, input, r, out);
    let used = bottom.min_rect().height();
    if (used - st.preview.bottom_h).abs() > 0.5 {
        st.preview.bottom_h = used;
        ui.ctx().request_repaint();
    }
}

fn empty_pane(ui: &mut Ui, _ink: &Ink, title: &str, sub: &str) {
    let pal = Palette::of_ui(ui);
    crate::widgets::empty_state(ui, ICON_LIST_TREE, pal.weak, title, sub);
}

/// Head band: gradient with a slanted bottom-right cut, kind icon tile, title, caption, label tag.
fn pane_head(ui: &mut Ui, ink: &Ink, node: &deck_core::tree::TreeNode, label: Option<&str>) {
    let pal = ink.p;
    let (title, icon, tint) = kind_title(node);
    // Reserve the band FIRST (painted below): content lays out inside it afterwards.
    let avail = ui.available_size();
    let band = Rect::from_min_size(ui.cursor().min, vec2(avail.x, 34.0 + 24.0));
    let bg = chat_paint::reserve(ui.painter());
    ui.allocate_space(vec2(avail.x, band.height()));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(band.shrink2(vec2(16.0, 12.0))));
    let resp = Frame::NONE.show(&mut child, |ui| {
        ui.set_width(ui.available_width());
        Sides::new().height(34.0).show(
            ui,
            |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                card::icon_tile(ui, 30.0, icon, tint, false);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(theme::strong(title).size(14.5).color(pal.text_strong));
                    ui.label(theme::section_caption("Selected entry", pal.weak));
                });
            },
            |ui| {
                if let Some(l) = label {
                    tag(ui, ink, &clip(l, 28), pal.warn);
                }
            },
        );
    });
    // Slanted bottom-right cut: the mockup's `.panehead` clip.
    let _ = &resp;
    let pts = vec![
        band.left_top(),
        band.right_top(),
        pos2(band.right(), band.bottom() - 10.0),
        pos2(band.right() - 15.0, band.bottom()),
        band.left_bottom(),
    ];
    ui.painter().set(bg, head_fill_shape(band, &pts, ink));
    ui.painter().hline(
        band.left()..=band.right() - 15.0,
        band.bottom(),
        Stroke::new(1.0, ink.line()),
    );
}

/// The head band's fill as a settable shape: flat fill in flat styles, a horizontal `head()` a→b
/// gradient otherwise (the same stops [`chat_paint::fill_h`] would paint).
fn head_fill_shape(rect: Rect, pts: &[Pos2], ink: &Ink) -> egui::Shape {
    if theme::flat() {
        return egui::Shape::convex_polygon(pts.to_vec(), ink.p.raised, Stroke::NONE);
    }
    let (a, b) = ink.head();
    let span = rect.width().max(1.0);
    let col = |p: Pos2| a.lerp_to_gamma(b, ((p.x - rect.left()) / span).clamp(0.0, 1.0));
    let c = rect.center();
    let mut m = egui::Mesh::default();
    m.colored_vertex(c, col(c));
    for p in pts {
        m.colored_vertex(*p, col(*p));
    }
    let n = pts.len() as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    egui::Shape::mesh(m)
}

/// Kind title, icon and tint of a node.
fn kind_title(node: &deck_core::tree::TreeNode) -> (String, crate::icons::Icon, Color32) {
    let plain = |t: &str, i: crate::icons::Icon| (t.to_owned(), i, Color32::TRANSPARENT);
    match &node.kind {
        NodeKind::User => plain("User prompt", ICON_USER),
        NodeKind::Assistant => plain("Assistant reply", ICON_SPARKLES),
        NodeKind::ToolResult => match node.tool_name.as_deref() {
            Some(name) => plain(&format!("Tool result · {name}"), ICON_WRENCH),
            None => plain("Tool result", ICON_WRENCH),
        },
        NodeKind::BashExecution => plain("Bash execution", ICON_WRENCH),
        NodeKind::Developer => plain("Developer note", ICON_SCROLL_TEXT),
        NodeKind::Compaction => plain("Compaction", ICON_FOLD_VERTICAL),
        NodeKind::BranchSummary => plain("Branch summary", ICON_SCROLL_TEXT),
        NodeKind::ModelChange => plain("Model change", ICON_CROSSHAIR),
        NodeKind::CustomMessage(t) | NodeKind::Custom(t) => plain(t, ICON_MESSAGES_SQUARE),
        NodeKind::Label => plain("Label", ICON_BOOKMARK),
        NodeKind::ModelUsage => plain("Model usage", ICON_COINS),
        NodeKind::ThinkingChange => plain("Thinking change", ICON_SPARKLES),
        NodeKind::OtherRole(role) | NodeKind::Setting(role) => plain(role, ICON_LIST_TREE),
    }
}

fn tag(ui: &mut Ui, ink: &Ink, text: &str, color: Color32) {
    let pal = ink.p;
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::label_font(11.0), pal.text);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 22.0), Sense::hover());
    let fill = if theme::flat() {
        pal.raised
    } else {
        ink.wash(color)
    };
    chat_paint::boxed(
        ui.painter(),
        rect,
        4,
        fill,
        crate::badge::outline(ui.pixels_per_point(), ink.edge(color)),
    );
    ui.painter().galley(
        rect.center() - g.size() / 2.0,
        g,
        ink.legible(color, fill, 3.0),
    );
}

/// Meta row of key/value badges: TIME, COST BEFORE, MODEL, ENTRY. Returns whether the ENTRY badge
/// was clicked (the caller copies the full id).
fn meta_row(
    ui: &mut Ui,
    ink: &Ink,
    st: &TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    node_idx: usize,
) -> bool {
    let pal = ink.p;
    let node = &tree.nodes[node_idx];
    let ts = node.timestamp.as_deref().and_then(tree_layout::parse_iso);
    let row_ts = st
        .cache
        .as_ref()
        .and_then(|c| {
            c.layout
                .row_of_node(node_idx)
                .and_then(|r| c.layout.rows.get(r)?.ts)
        })
        .or(ts);
    let model = cached_model(st, &node.id);
    let short = short_id(&node.id);
    let mut copy_id = false;
    Frame::NONE
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(14.0, 8.0);
                if let Some(ts) = row_ts {
                    let hhmm = clock_hhmm(ts);
                    kv_badge(
                        ui,
                        ink,
                        "Time",
                        &format!("{hhmm} · {} ago", tree_layout::age(input.now, ts)),
                        ValueStyle::Plain,
                        None,
                    );
                }
                kv_badge(
                    ui,
                    ink,
                    "Cost before",
                    &cost_amount(node.cost_before),
                    ValueStyle::Cost,
                    Some("Journal order across all branches; excludes this entry and unrecorded live subagent spend."),
                );
                if let Some(model) = model.as_deref() {
                    kv_badge(ui, ink, "Model", model, ValueStyle::Plain, None);
                }
                // ENTRY: short id; click copies the full id.
                if kv_badge_button(ui, ink, "Entry", &short) {
                    copy_id = true;
                }
            });
        });
    let y = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().left()..=ui.max_rect().right(),
        y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.7)),
    );
    copy_id
}

fn clock_hhmm(ts: i64) -> String {
    let clock = tree_layout::clock(ts);
    // `tree_layout::clock` gives `2026-10-21 12:34:56 UTC`; the meta row shows `HH:MM`.
    clock
        .split(' ')
        .nth(1)
        .and_then(|t| t.get(..5))
        .unwrap_or(&clock)
        .to_owned()
}

fn short_id(id: &str) -> String {
    const KEEP: usize = 8;
    if id.len() <= KEEP + 3 {
        id.to_owned()
    } else {
        format!("{}…", &id[..KEEP])
    }
}

/// The model of the entry's assistant item, when the cached detail has one.
fn cached_model(st: &TreeState, id: &str) -> Option<String> {
    let data = st.preview.cache.get(id)?;
    data.items.iter().find_map(|it| match it {
        TranscriptItem::Assistant { model, .. } => model.clone().filter(|m| !m.is_empty()),
        _ => None,
    })
}

#[derive(Clone, Copy)]
enum ValueStyle {
    Plain,
    Cost,
}

fn kv_badge(
    ui: &mut Ui,
    ink: &Ink,
    key: &str,
    value: &str,
    style: ValueStyle,
    tooltip: Option<&str>,
) {
    let pal = ink.p;
    let kg = ui
        .painter()
        .layout_no_wrap(key.to_uppercase(), theme::label_font(9.5), pal.weak);
    let vg = ui
        .painter()
        .layout_no_wrap(value.to_owned(), theme::label_font(11.0), pal.text);
    let h = 22.0;
    let (kw, vw) = (kg.size().x, vg.size().x);
    let w = kw + 6.0 + vw + 14.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    p.galley(
        pos2(rect.left(), rect.center().y - kg.size().y / 2.0),
        kg,
        pal.weak,
    );
    let vrect = Rect::from_min_size(pos2(rect.left() + kw + 6.0, rect.top()), vec2(vw + 14.0, h));
    let (fill, edge, fg) = match style {
        ValueStyle::Plain => (ink.chip(), ink.line(), pal.text),
        ValueStyle::Cost => {
            let fill = if theme::flat() {
                pal.raised
            } else {
                ink.wash(pal.ok)
            };
            (fill, ink.edge(pal.ok), ink.legible(pal.ok, fill, 3.0))
        }
    };
    chat_paint::boxed(
        p,
        vrect,
        4,
        fill,
        crate::badge::outline(p.pixels_per_point(), edge),
    );
    p.galley(vrect.center() - vg.size() / 2.0, vg, fg);
    if let Some(tip) = tooltip {
        resp.on_hover_text(tip);
    }
}

fn kv_badge_button(ui: &mut Ui, ink: &Ink, key: &str, value: &str) -> bool {
    let pal = ink.p;
    let kg = ui
        .painter()
        .layout_no_wrap(key.to_uppercase(), theme::label_font(9.5), pal.weak);
    let vg = ui
        .painter()
        .layout_no_wrap(value.to_owned(), theme::label_font(11.0), pal.text);
    let h = 22.0;
    let (kw, vw) = (kg.size().x, vg.size().x);
    let w = kw + 6.0 + vw + 14.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let p = ui.painter();
    p.galley(
        pos2(rect.left(), rect.center().y - kg.size().y / 2.0),
        kg,
        pal.weak,
    );
    let vrect = Rect::from_min_size(pos2(rect.left() + kw + 6.0, rect.top()), vec2(vw + 14.0, h));
    let hov = resp.hovered();
    chat_paint::boxed(
        p,
        vrect,
        4,
        ink.chip(),
        Stroke::new(1.0, if hov { ink.a } else { ink.line() }),
    );
    p.galley(vrect.center() - vg.size() / 2.0, vg, pal.text);
    resp.on_hover_text("Copy the full entry id")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
}

/// The scrollable reading area: the entry's items chat-style, a spinner while loading, errors
/// inline, the row text for empty non-message kinds.
fn reading(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    node_idx: usize,
    height: f32,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    let node = &tree.nodes[node_idx];
    let pv = &st.preview;
    let loading = pv.inflight.as_ref().is_some_and(|f| f.id == node.id);
    let stale_gateway = node.line.is_none();
    let failed = pv.invalid.get(&node.id).is_some_and(|(_, n)| *n >= 2);
    Frame::NONE
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // `height` is the outer height: minus the frame's 10 + 10 vertical margin.
            ui.set_height((height - 20.0).max(40.0));
            // The entry's line never moves, so the cache needs no revision check.
            let cached = st.preview.cache.contains_key(&node.id);
            if loading && !cached {
                ui.vertical_centered(|ui| {
                    ui.add_space(height * 0.3);
                    ui.add(crate::widgets::Spin::new(18.0, pal.accent));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Reading the entry…")
                            .color(pal.weak)
                            .size(12.0),
                    );
                });
                return;
            }
            let has = st.preview.cache.contains_key(&node.id);
            if !has {
                if loading {
                    ui.vertical_centered(|ui| {
                        ui.add(crate::widgets::Spin::new(18.0, pal.accent));
                    });
                } else if stale_gateway {
                    gateway_note(ui, *pal, &node.text, &node.kind);
                } else if failed {
                    ui.label(
                        RichText::new(
                            "The session file changed while reading; reopen the tree to retry.",
                        )
                        .color(pal.warn),
                    );
                } else {
                    ui.label(RichText::new("Select a row to preview its entry.").color(pal.weak));
                }
                return;
            };
            let data = st.preview.cache.get(&node.id).expect("checked above");
            if let Some(e) = data.error.clone() {
                ui.label(RichText::new(format!("Cannot read the entry: {e}")).color(pal.danger));
                return;
            }
            if data.truncated {
                ui.label(RichText::new("Entry too large to preview (over 8 MiB).").color(pal.warn));
                return;
            }
            if data.items.is_empty() {
                // Non-message kinds: the row text.
                if data.row_text.is_empty() {
                    ui.label(
                        RichText::new(tree_layout::kind_name(&node.kind))
                            .color(pal.text)
                            .size(13.0),
                    );
                } else {
                    ui.label(RichText::new(&data.row_text).color(pal.text).size(13.0));
                }
                return;
            }
            // A byline for assistant replies: `π omp · <model>` + time, hairline under.
            let model = data.items.iter().find_map(|it| match it {
                TranscriptItem::Assistant { model, .. } => model.clone(),
                _ => None,
            });
            if matches!(node.kind, NodeKind::Assistant) {
                byline(ui, model.as_deref(), node);
            }
            // Sync the reading view only when the shown entry changes; the items are already an
            // `Arc`, so this clones one pointer, not the transcript.
            let pv = &mut st.preview;
            if pv.synced.as_deref() != Some(node.id.as_str()) {
                let items =
                    std::sync::Arc::clone(&pv.cache.get(&node.id).expect("checked above").items);
                let id = node.id.clone();
                pv.view.sync(preview_rev(&id), || items);
                pv.synced = Some(id);
            }
            // The transcript view keeps its own scroll area; give it the remaining height.
            let h = ui.available_height();
            ui.set_height(h.max(60.0));
            let path = input.session.unwrap_or(std::path::Path::new(""));
            if let Some(url) = pv.view.show(ui, false, path) {
                out.push(TreeAction::OpenUrl(url));
            }
        });
}

/// Row text plus an upgrade note: the gateway is older than the GUI and sent a tree without
/// entry lines, so entries cannot be read.
fn gateway_note(ui: &mut Ui, pal: Palette, row_text: &str, kind: &NodeKind) {
    let text = if row_text.is_empty() {
        tree_layout::kind_name(kind)
    } else {
        row_text
    };
    ui.label(RichText::new(text).color(pal.text).size(13.0));
    ui.add_space(8.0);
    ui.label(
        RichText::new("Full content needs the updated OmpDeck gateway — restart OmpDeck.")
            .color(pal.weak)
            .size(11.5),
    );
}

/// Revision key for the reading view: one per (node id, tree revision).
fn preview_rev(id: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

fn byline(ui: &mut Ui, model: Option<&str>, node: &deck_core::tree::TreeNode) {
    let pal = Palette::of_ui(ui);
    let ink = Ink::new(ui, 12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        chat_paint::omp_mark(ui.painter(), rect, &ink);
        let mut line = "omp".to_owned();
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            line.push_str(" · ");
            line.push_str(&crate::agent_model::short_model(m));
        }
        ui.label(RichText::new(line).size(11.0).color(pal.weak));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if let Some(ts) = node.timestamp.as_deref().and_then(tree_layout::parse_iso) {
                ui.label(RichText::new(clock_hhmm(ts)).size(11.0).color(pal.weak));
            }
        });
    });
    let y = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().left()..=ui.max_rect().right(),
        y,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.7)),
    );
    ui.add_space(6.0);
}

/// Plain text of the entry for the Copy button.
fn copy_text_of(items: &[TranscriptItem], row_text: &str) -> String {
    if items.is_empty() {
        return row_text.to_owned();
    }
    let mut parts = Vec::new();
    for it in items {
        match it {
            TranscriptItem::User { text, .. }
            | TranscriptItem::Assistant { text, .. }
            | TranscriptItem::Thinking { text }
            | TranscriptItem::Notice { text, .. }
            | TranscriptItem::Irc { text, .. } => parts.push(text.clone()),
            TranscriptItem::ToolCall { summary, args, .. } => {
                if !summary.is_empty() {
                    parts.push(summary.clone());
                }
                if !args.trim().is_empty() {
                    parts.push(args.clone());
                }
            }
            TranscriptItem::ToolResult { text, .. } => parts.push(text.clone()),
            TranscriptItem::Compaction { summary, .. } => parts.push(summary.clone()),
            TranscriptItem::Error { message } => parts.push(message.clone()),
            TranscriptItem::ModelChange { model } => parts.push(format!("Model → {model}")),
            TranscriptItem::Exit { reason } => parts.push(reason.clone()),
        }
    }
    let joined = parts.join("\n\n");
    if joined.trim().is_empty() {
        row_text.to_owned()
    } else {
        joined
    }
}

/// The label editor in the preview pane (omp edits labels in the preview).
fn label_editor(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    let Some(edit) = st.label.as_mut() else {
        return;
    };
    let mut save = false;
    let mut cancel = false;
    Frame::NONE
        .inner_margin(Margin::symmetric(16, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let y = ui.cursor().top();
            ui.painter().hline(
                ui.max_rect().left() - 16.0..=ui.max_rect().right() + 16.0,
                y - 8.0,
                Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            );
            ui.horizontal(|ui| {
                ui.set_min_height(crate::widgets::draft_height());
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.label(crate::widgets::icon_text(ICON_BOOKMARK, 16.0, pal.warn));
                ui.label(RichText::new("Label").size(13.0).color(pal.text_strong));
                let width = (ui.available_width() - 232.0).max(120.0);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut edit.text)
                        .hint_text("empty removes it")
                        .desired_width(width)
                        .margin(vec2(10.0, 6.0)),
                );
                if std::mem::take(&mut edit.focus) {
                    field.request_focus();
                }
                let ready = input.label.is_ok();
                if DraftButton::new("Save")
                    .kind(DraftKind::Primary)
                    .enabled(ready)
                    .min_width(72.0)
                    .show(ui)
                    .clicked()
                {
                    save = true;
                }
                if DraftButton::new("Cancel")
                    .min_width(72.0)
                    .show(ui)
                    .clicked()
                {
                    cancel = true;
                }
            });
        });
    if cancel {
        st.label = None;
    } else if save {
        finish_label(st, input, out);
    }
}

/// The action bar at the bottom of the pane: Go here / Summarize & go / Fork / Label /
/// Re-answer / Copy.
fn action_bar(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(pal.bar.gamma_multiply(0.6))
        .inner_margin(Margin::symmetric(16, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let y = ui.cursor().top();
            ui.painter().hline(
                ui.max_rect().left() - 16.0..=ui.max_rect().right() + 16.0,
                y - 10.0,
                Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            );
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(7.0, 6.0);
                let Some(c) = st.cache.as_ref() else {
                    return;
                };
                let go_off = go_blocked(&c.layout, input, r);
                let node = c.layout.rows[r].node;
                let fork_off = fork_blocked(tree, &c.layout, input, r);
                let re_off = reanswer_blocked(tree, &c.layout, input, r);
                let label_off = input.label.err();
                let go_tip = if matches!(tree.nodes[node].kind, NodeKind::User) {
                    "Go to just before this prompt; its text goes back to the editor (Enter)"
                } else {
                    "Continue from here in this tab (Enter)"
                };
                if action_btn(
                    ui,
                    "Go here",
                    ICON_CROSSHAIR,
                    DraftKind::Primary,
                    go_off,
                    go_tip,
                )
                .clicked()
                    && go_off.is_none()
                {
                    crate::tree_view::go_here(st, tree, input, r, false, out);
                }
                if action_btn(
                    ui,
                    "Summarize & go",
                    ICON_SCROLL_TEXT,
                    DraftKind::Secondary,
                    go_off,
                    "Go here with a summary of the branch left (Shift+Enter)",
                )
                .clicked()
                    && go_off.is_none()
                {
                    // Shift+Enter's plan without the dialog.
                    match go_plan(input.summary_prompt, true) {
                        GoPlan::Now(summary) => {
                            let n = &tree.nodes[node];
                            out.push(TreeAction::Navigate {
                                entry: n.id.clone(),
                                user: matches!(n.kind, NodeKind::User),
                                summary,
                            });
                        }
                        GoPlan::Ask => crate::tree_view::go_here(st, tree, input, r, true, out),
                    }
                }
                if action_btn(
                    ui,
                    "Fork",
                    ICON_GIT_FORK,
                    DraftKind::Secondary,
                    fork_off,
                    tree_layout::fork_hint(&tree.nodes[node].kind),
                )
                .clicked()
                    && fork_off.is_none()
                {
                    crate::tree_view::fork_here(st, tree, input, r, out);
                }
                if action_btn(
                    ui,
                    "Label",
                    ICON_BOOKMARK,
                    DraftKind::Secondary,
                    label_off,
                    "Edit this entry's label (L)",
                )
                .clicked()
                    && label_off.is_none()
                {
                    label_here(st, tree, input, r, out);
                }
                if action_btn(
                    ui,
                    "Re-answer",
                    ICON_MESSAGES_SQUARE,
                    DraftKind::Secondary,
                    re_off,
                    "Re-answer: omp asks the question again in the tab",
                )
                .clicked()
                    && re_off.is_none()
                {
                    crate::tree_view::reanswer_here(st, tree, input, r, out);
                }
                if action_btn(
                    ui,
                    "Copy",
                    ICON_COPY,
                    DraftKind::Secondary,
                    None,
                    "Copy the entry text",
                )
                .clicked()
                {
                    crate::tree_view::copy_row(st, tree, r, out);
                }
            });
        });
}

/// One action-bar button: a DraftButton with the gate reason as its disabled tooltip.
fn action_btn(
    ui: &mut Ui,
    label: &str,
    icon: crate::icons::Icon,
    kind: DraftKind,
    off: Option<&str>,
    tip: &str,
) -> egui::Response {
    let resp = DraftButton::new(label)
        .kind(kind)
        .icon(icon)
        .enabled(off.is_none())
        .show(ui);
    match off {
        Some(why) => resp.on_disabled_hover_text(why),
        None => resp.on_hover_text(tip),
    }
}

#[cfg(test)]
impl PreviewState {
    pub fn test_inflight(&self) -> bool {
        self.inflight.is_some()
    }

    pub fn test_cached(&self, id: &str) -> bool {
        self.cache.contains_key(id)
    }

    /// Feed a worker answer directly (as if it arrived over the channel).
    pub fn test_answer(&mut self, id: &str, revision: u64, result: std::io::Result<EntryDetail>) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(PreviewAnswer {
            id: id.to_owned(),
            revision,
            result,
        })
        .expect("send test answer");
        self.tx = Some(tx);
        self.rx = Some(rx);
    }
}
