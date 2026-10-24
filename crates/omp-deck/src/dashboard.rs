//! The agents dashboard view: a responsive grid of session cards over the terminal area.
//!
//! The pure parts (what a card says, search/filter/order, grid geometry) live in
//! `dashboard_model.rs`; this module owns the cached state and paints it. Cards are painted
//! directly and culled to the viewport; their heights are a pure function of the data, so the
//! scroll range is exact without measuring while painting.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use deck_core::agents::{AgentSummary, AgentsSnapshot};
use deck_core::ctl::LiveSnapshot;
use deck_core::fleet::FleetSnapshot;
use egui::text::LayoutJob;
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Galley, Id, Margin, Painter, Rect, RichText,
    ScrollArea, Sense, Sides, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use crate::agent_ctl::{CtlView, Link, Restart};
use crate::agent_hub::{self, RowAction, RowsInput, RowsState};
use crate::agent_model::AgentsView;
use crate::dashboard_model::{
    Card, CardKey, FILTERS, GAP, Grid, Item, PAD, Query, REPLY_LINE_H, REPLY_LINES, ROW_AGENTS,
    ROW_CTX, ROW_FILES, ROW_HEAD, ROW_MODEL, ROW_PROJECT, SECTION_GAP, Status, StatusFilter,
    card_height, columns, layout, visible,
};
use crate::icons::{
    ICON_BRAIN, ICON_CHECK, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_FOLDER, ICON_FOLDER_GIT_2,
    ICON_GIT_BRANCH, ICON_LIST, ICON_LIST_TREE, ICON_NETWORK, ICON_SEARCH, ICON_X, Icon,
};
use crate::statusbar::level_color;
use crate::statusfmt::ContextLevel;
use crate::tabs::paint_status;
use crate::theme::{self, Palette, acc, skin};
use crate::widgets::{Spin, empty_state, icon_text, paint_icon};
use deck_core::config::UiStyle;

/// Cards this far outside the viewport are still painted (smooth scrolling).
const OVERSCAN: f32 = 240.0;
/// Height of the live-control note inside an expanded card.
const NOTE_H: f32 = 32.0;

/// One session's subagents: the snapshot built from the fleet, its formatted view and the rows.
pub struct SessionAgents {
    /// The fleet's list this snapshot was copied from: same `Arc` = unchanged.
    src: Arc<Vec<AgentSummary>>,
    snap: Arc<AgentsSnapshot>,
    pub view: AgentsView,
    pub rows: RowsState,
}

/// Dashboard state owned by the app.
pub struct DashState {
    pub open: bool,
    pub query: String,
    pub filter: StatusFilter,
    /// Subagent rows as a tree (who spawned whom) or a flat list.
    pub tree: bool,
    expanded: HashSet<CardKey>,
    expanded_ver: u64,
    /// Scroll this card into view on the next frame.
    scroll_to: Option<CardKey>,
    focus_search: bool,
    pub cards: Vec<Card>,
    cards_ver: u64,
    pub sessions: HashMap<PathBuf, SessionAgents>,
    /// The fleet snapshot the cards were built from (kept so its address stays unique).
    pub fleet: Option<Arc<FleetSnapshot>>,
    /// Hash of everything the cards were built from; unchanged → no rebuild.
    pub sig: Option<u64>,
    counts: [usize; 4],
    order: Vec<usize>,
    order_key: Option<u64>,
    grid: Grid,
    grid_key: Option<u64>,
}

impl Default for DashState {
    fn default() -> Self {
        DashState {
            open: false,
            query: String::new(),
            filter: StatusFilter::All,
            tree: true,
            expanded: HashSet::new(),
            expanded_ver: 0,
            scroll_to: None,
            focus_search: false,
            cards: Vec::new(),
            cards_ver: 0,
            sessions: HashMap::new(),
            fleet: None,
            sig: None,
            counts: [0; 4],
            order: Vec::new(),
            order_key: None,
            grid: Grid::default(),
            grid_key: None,
        }
    }
}

fn hash_of(x: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    x.hash(&mut h);
    h.finish()
}

impl DashState {
    /// Shows the dashboard; `focus` = the card to expand and scroll into view.
    pub fn show_with(&mut self, focus: Option<CardKey>) {
        self.open = true;
        self.focus_search = true;
        if let Some(key) = focus {
            if self.expanded.insert(key.clone()) {
                self.expanded_ver += 1;
            }
            self.scroll_to = Some(key);
        }
    }

    /// Hides the dashboard and drops everything derived from the fleet.
    pub fn close(&mut self) {
        self.open = false;
        self.cards.clear();
        self.sessions.clear();
        self.fleet = None;
        self.sig = None;
        self.order.clear();
        self.order_key = None;
        self.grid = Grid::default();
        self.grid_key = None;
        self.scroll_to = None;
        self.counts = [0; 4];
    }

    /// Installs freshly built cards (and the fleet they came from).
    pub fn set_cards(&mut self, cards: Vec<Card>, fleet: Option<Arc<FleetSnapshot>>, sig: u64) {
        self.counts = FILTERS.map(|f| cards.iter().filter(|c| f.admits(c.status)).count());
        self.cards = cards;
        self.cards_ver += 1;
        self.sig = Some(sig);
        self.sync_sessions(fleet.as_ref());
        self.fleet = fleet;
    }

    /// One [`SessionAgents`] per reported session; unchanged agent lists keep their `Arc`.
    fn sync_sessions(&mut self, fleet: Option<&Arc<FleetSnapshot>>) {
        let Some(fleet) = fleet else {
            self.sessions.clear();
            return;
        };
        self.sessions
            .retain(|p, _| fleet.sessions.iter().any(|s| &s.session == p));
        for s in &fleet.sessions {
            let fresh = || {
                Arc::new(AgentsSnapshot {
                    session: s.session.clone(),
                    agents: s.agents.as_ref().clone(),
                })
            };
            match self.sessions.get_mut(&s.session) {
                Some(sa) => {
                    if !Arc::ptr_eq(&sa.src, &s.agents) {
                        sa.src = Arc::clone(&s.agents);
                        sa.snap = fresh();
                    }
                }
                None => {
                    self.sessions.insert(
                        s.session.clone(),
                        SessionAgents {
                            src: Arc::clone(&s.agents),
                            snap: fresh(),
                            view: AgentsView::default(),
                            rows: RowsState::default(),
                        },
                    );
                }
            }
        }
    }

    /// Refreshes the formatted views; `active` = the active tab's session with omp's own live
    /// state of its agents.
    pub fn update_views(
        &mut self,
        active: Option<(&Path, Option<&Arc<LiveSnapshot>>)>,
        now: SystemTime,
    ) {
        for (path, sa) in &mut self.sessions {
            let live = active
                .filter(|(p, _)| *p == path.as_path())
                .and_then(|(_, l)| l);
            sa.view.update(Some(&sa.snap), live, now);
        }
    }

    /// Changes whenever any session's subagent view did.
    fn agents_epoch(&self) -> u64 {
        self.sessions
            .values()
            .fold(self.sessions.len() as u64, |a, s| {
                a.wrapping_add(s.view.version)
            })
    }

    fn sync_order(&mut self) {
        let key = hash_of(&(
            self.cards_ver,
            self.agents_epoch(),
            &self.query,
            self.filter,
        ));
        if self.order_key == Some(key) {
            return;
        }
        let q = Query::new(&self.query);
        let sessions = &self.sessions;
        self.order = visible(&self.cards, &q, self.filter, |c| {
            c.session
                .as_deref()
                .and_then(|p| sessions.get(p))
                .map_or(&[][..], |s| s.view.search.as_slice())
        });
        self.order_key = Some(key);
    }
}

pub enum DashAction {
    Close,
    /// Open or focus this card's session (and leave the dashboard).
    OpenCard(CardKey),
    /// A subagent row was clicked.
    Agent {
        session: PathBuf,
        id: String,
    },
    Kill(String),
    Revive(String),
    /// Restart the active session with live control.
    Restart,
}

pub struct DashInput<'a> {
    /// Control of the active tab's session (the only one with a live link).
    pub ctl_active: &'a CtlView<'a>,
    /// Read-only stand-in for every other session.
    pub ctl_idle: &'a CtlView<'a>,
    /// The subagent whose popup is open: `(session, id)`.
    pub popup: Option<(&'a Path, &'a str)>,
}

/// Draws the whole dashboard into the current (central) ui.
pub fn show(ui: &mut Ui, st: &mut DashState, input: &DashInput<'_>) -> Vec<DashAction> {
    let pal = Palette::of_ui(ui);
    let mut out = Vec::new();
    st.sync_order();
    theme::skin::paint_canvas(ui.painter(), ui.max_rect(), pal);
    Frame::NONE
        .inner_margin(Margin::symmetric(18, 14))
        .show(ui, |ui| {
            header(ui, st, &mut out);
            ui.add_space(12.0);
            if st.cards.is_empty() {
                empty_state(
                    ui,
                    ICON_NETWORK,
                    pal.accent,
                    "No sessions to show",
                    "Open a session from the sidebar or press + next to a project; running sessions appear here live.",
                );
            } else if st.order.is_empty() {
                empty_state(
                    ui,
                    ICON_SEARCH,
                    pal.weak,
                    "Nothing matches",
                    "Try another search or another status filter.",
                );
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if ui.button("Clear filters").clicked() {
                        st.query.clear();
                        st.filter = StatusFilter::All;
                    }
                });
            } else {
                grid(ui, st, input, &mut out);
            }
        });
    out
}

// ------------------------------------------------------------------ header

fn header(ui: &mut Ui, st: &mut DashState, out: &mut Vec<DashAction>) {
    let pal = Palette::of_ui(ui);
    Sides::new().height(30.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(icon_text(ICON_NETWORK, 20.0, pal.glyph));
            if theme::style() == UiStyle::Phosphor {
                ui.label(
                    RichText::new("> ")
                        .font(theme::label_font(17.0))
                        .color(pal.accent),
                );
                ui.add_space(-8.0);
                ui.label(
                    RichText::new("agents")
                        .font(theme::label_font(17.0))
                        .color(pal.text_strong),
                );
            } else if theme::look().mono_labels {
                ui.label(
                    RichText::new("AGENTS")
                        .font(theme::label_font(15.0))
                        .extra_letter_spacing(1.6)
                        .color(pal.text_strong),
                );
            } else {
                ui.label(theme::strong("Agents").size(17.0).color(pal.text_strong));
            }
            if !st.cards.is_empty() {
                pill(ui, &st.cards.len().to_string(), pal.weak, pal.raised);
            }
            if st.fleet.is_none() && st.cards.iter().any(|c| c.session.is_some()) {
                ui.add(Spin::new(14.0, pal.accent));
                ui.label(
                    RichText::new("Reading sessions…")
                        .size(11.5)
                        .color(pal.weak),
                );
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if small_button(ui, ICON_X, "Close (Esc)", false).clicked() {
                out.push(DashAction::Close);
            }
            ui.add_space(4.0);
            if small_button(
                ui,
                ICON_LIST,
                "Subagents as a flat list: running first, newest activity",
                !st.tree,
            )
            .clicked()
            {
                st.tree = false;
            }
            if small_button(
                ui,
                ICON_LIST_TREE,
                "Subagents as a tree: who spawned whom",
                st.tree,
            )
            .clicked()
            {
                st.tree = true;
            }
        },
    );
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        search_box(ui, st);
        for (i, f) in FILTERS.iter().enumerate() {
            if filter_chip(ui, f.label(), st.counts[i], st.filter == *f).clicked() {
                st.filter = *f;
            }
        }
    });
}

fn search_box(ui: &mut Ui, st: &mut DashState) {
    let pal = Palette::of_ui(ui);
    let id = Id::unique("dashboard-search");
    if st.focus_search {
        st.focus_search = false;
        ui.memory_mut(|m| m.request_focus(id));
    }
    let focused = ui.memory(|m| m.has_focus(id));
    ui.add(
        TextEdit::singleline(&mut st.query)
            .id(id)
            .prefix(icon_text(ICON_SEARCH, 16.0, pal.weak))
            .hint_text("Search title, project, id, subagent…")
            .align(Align2::LEFT_CENTER)
            .min_size(vec2(0.0, 30.0))
            .desired_width(340.0)
            .margin(Margin::symmetric(10, 6))
            .frame(
                Frame::new()
                    .fill(pal.surface)
                    .corner_radius(theme::ui_rad(theme::rad(8, 10)))
                    .stroke(Stroke::new(
                        1.0,
                        if focused { pal.accent } else { pal.stroke },
                    )),
            ),
    );
}

fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::label_font(11.5), fg);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 18.0), Sense::hover());
    skin::paint_chip(ui.painter(), rect, theme::ui_rad(9), pal, bg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
}

fn small_button(ui: &mut Ui, icon: Icon, tip: &str, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() || on, 0.1);
    let p = ui.painter();
    p.rect_filled(
        rect,
        theme::rad(6, 8),
        if on {
            theme::accent_fill(pal, 0.28)
        } else {
            pal.raised.gamma_multiply(0.85 * hov)
        },
    );
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        17.0,
        if on {
            pal.text_strong
        } else {
            theme::mix(pal.weak, pal.text_strong, hov)
        },
    );
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// Background of a filter pill that carries a gradient border.
fn mix_fill(pal: &Palette) -> Color32 {
    theme::mix(pal.surface, pal.accent, 0.1)
}

fn filter_chip(ui: &mut Ui, label: &str, n: usize, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let color = if on { pal.accent } else { pal.weak };
    let g = ui.painter().layout_no_wrap(
        format!("{label}  {n}"),
        theme::font(12.0),
        if on { pal.text_strong } else { pal.text },
    );
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 24.0, 28.0), Sense::click());
    let p = ui.painter();
    let radius = theme::ui_rad(14);
    let fill = if on {
        color.gamma_multiply(0.2)
    } else if resp.hovered() {
        pal.hover_bg
    } else {
        pal.surface
    };
    let stroke = Stroke::new(
        1.0,
        if on {
            color
        } else if resp.hovered() {
            pal.hover_line
        } else {
            pal.stroke
        },
    );
    if on && theme::look().grad_accent {
        skin::gradient_outline(p, resp.id, rect, f32::from(radius), pal, mix_fill(pal));
    } else {
        p.rect_filled(rect, radius, fill);
        p.rect_stroke(rect, radius, stroke, StrokeKind::Inside);
    }
    skin::paint_field_highlight(p, rect, f32::from(radius));
    p.galley(
        pos2(rect.left() + 12.0, rect.center().y - g.size().y / 2.0),
        g,
        pal.text,
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ grid

/// Why the active session cannot be controlled (one line), if it cannot.
fn link_note_text(link: Link) -> Option<&'static str> {
    match link {
        Link::ReadOnly => Some("Started without live control: agents are read-only."),
        Link::Lost => Some("Live control disconnected: agents are read-only for now."),
        Link::Ended => Some("This session is no longer running."),
        Link::Live | Link::Connecting => None,
    }
}

fn grid(ui: &mut Ui, st: &mut DashState, input: &DashInput<'_>, out: &mut Vec<DashAction>) {
    let DashState {
        cards,
        sessions,
        expanded,
        expanded_ver,
        order,
        grid: placed,
        grid_key,
        tree,
        cards_ver,
        scroll_to,
        ..
    } = st;
    let tree = *tree;
    let mut toggled: Option<CardKey> = None;
    ScrollArea::vertical()
        .id_salt("dashboard-grid")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let width = ui.available_width();
            let cols = columns(width);
            let card_w = (width - GAP * (cols - 1) as f32) / cols as f32;
            let g = acc::geo(PAD);
            let inner_w = width - g.rows_l - g.rows_r;
            let note = link_note_text(input.ctl_active.link);
            // Geometry: recomputed only when something it depends on changed.
            let epoch = sessions
                .values()
                .fold(sessions.len() as u64, |a, s| a.wrapping_add(s.view.version));
            let key = hash_of(&(
                *cards_ver,
                epoch,
                order.as_slice(),
                *expanded_ver,
                width.round() as i32,
                tree,
                note.is_some(),
                ui.visuals().dark_mode,
                theme::style().key(),
            ));
            if *grid_key != Some(key) {
                let items: Vec<Item> = order
                    .iter()
                    .map(|&ci| {
                        let card = &cards[ci];
                        let sa = card.session.as_deref().and_then(|p| sessions.get_mut(p));
                        let has_agents = sa.as_ref().is_some_and(|s| s.view.counts.total() > 0);
                        let open = has_agents && expanded.contains(&card.key);
                        let mut height = card_height(card.shape(has_agents));
                        if has_agents {
                            height += (g.h - ROW_AGENTS) + (g.foot - PAD);
                        }
                        if open && let Some(sa) = sa {
                            let rows_h = sa.rows.prepare(ui, &sa.view, inner_w, tree);
                            let note_h = if card.active && note.is_some() {
                                NOTE_H + SECTION_GAP
                            } else {
                                0.0
                            };
                            height += g.pad_top + note_h + rows_h + g.pad_bottom;
                        }
                        Item { height, wide: open }
                    })
                    .collect();
                *placed = layout(&items, cols);
                *grid_key = Some(key);
            }
            let (area, _) = ui.allocate_exact_size(vec2(width, placed.total), Sense::hover());
            if let Some(target) = scroll_to.take()
                && let Some(pos) = order.iter().position(|&ci| cards[ci].key == target)
                && let Some(s) = placed.slots.get(pos)
            {
                let r = Rect::from_min_size(pos2(area.min.x, area.min.y + s.y), vec2(width, s.h));
                ui.scroll_to_rect(r, Some(Align::Min));
                ui.ctx().request_repaint();
            }
            let range = placed.visible(viewport.min.y - OVERSCAN, viewport.max.y + OVERSCAN);
            for pos in range {
                let (Some(&ci), Some(s)) = (order.get(pos), placed.slots.get(pos)) else {
                    continue;
                };
                let card = &cards[ci];
                let x = area.min.x + s.col as f32 * (card_w + GAP);
                let w = s.span as f32 * card_w + (s.span - 1) as f32 * GAP;
                let rect = Rect::from_min_size(pos2(x, area.min.y + s.y), vec2(w, s.h));
                let sa = card.session.as_deref().and_then(|p| sessions.get_mut(p));
                let open = sa
                    .as_ref()
                    .is_some_and(|s| s.view.counts.total() > 0 && expanded.contains(&card.key));
                let paint = CardPaint {
                    rect,
                    open,
                    tree,
                    ctl: if card.active {
                        input.ctl_active
                    } else {
                        input.ctl_idle
                    },
                    note: note.filter(|_| card.active),
                    popup: input.popup,
                };
                if let Some(k) = paint_card(ui, card, sa, &paint, out) {
                    toggled = Some(k);
                }
            }
        });
    if let Some(key) = toggled {
        if !expanded.remove(&key) {
            expanded.insert(key);
        }
        *expanded_ver += 1;
    }
}

struct CardPaint<'a> {
    rect: Rect,
    open: bool,
    tree: bool,
    ctl: &'a CtlView<'a>,
    note: Option<&'static str>,
    popup: Option<(&'a Path, &'a str)>,
}

fn status_color(pal: &Palette, s: Status) -> Color32 {
    match s {
        Status::Attention => pal.warn,
        Status::Busy => pal.accent_b,
        Status::Idle => pal.ok,
        Status::Running => pal.accent,
        Status::Exited { clean: true } => pal.weak,
        Status::Exited { clean: false } => pal.danger,
    }
}

/// One line of text cut with an ellipsis at `max_w`.
fn line(p: &Painter, text: &str, size: f32, color: Color32, max_w: f32) -> Arc<Galley> {
    // Chips and paths are mono where the style sets its labels in mono; titles are not.
    let font = if size < 13.0 {
        theme::label_font(size)
    } else {
        theme::font(size)
    };
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_w.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    p.layout_job(job)
}

/// A rounded chip with `text` ending at `right`; returns its left edge.
fn chip_right(p: &Painter, right: f32, cy: f32, text: &str, fg: Color32, bg: Color32) -> f32 {
    let g = p.layout_no_wrap(text.to_owned(), theme::label_font(11.0), fg);
    let w = g.size().x + 14.0;
    let r = Rect::from_min_size(pos2(right - w, cy - 9.0), vec2(w, 18.0));
    let radius = theme::ui_rad(9);
    if theme::style() == UiStyle::Phosphor {
        // Phosphor outlines its badges in the badge colour.
        p.rect_stroke(r, radius, Stroke::new(1.0, fg), StrokeKind::Inside);
    } else {
        p.rect_filled(r, radius, bg);
    }
    p.galley(pos2(r.left() + 7.0, cy - g.size().y / 2.0), g, fg);
    r.left()
}

/// Paints one card (and its expanded subagent rows); returns the card to toggle when its
/// subagents row was clicked.
fn paint_card(
    ui: &mut Ui,
    card: &Card,
    sa: Option<&mut SessionAgents>,
    c: &CardPaint<'_>,
    out: &mut Vec<DashAction>,
) -> Option<CardKey> {
    let pal = Palette::of_ui(ui);
    let id = Id::unique(("dashboard-card", &card.key));
    let resp = ui.interact(c.rect, id, Sense::click());
    let hovered = resp.hovered();
    let lk = theme::look();
    // Neo-Brutal lifts a hovered card by one pixel up and left (its hard shadow grows).
    let rect = if hovered && lk.card.hover == theme::Hover::Hard {
        c.rect.translate(vec2(-1.0, -1.0))
    } else {
        c.rect
    };
    let color = status_color(pal, card.status);
    let mut p = ui.painter_at(rect.expand(2.0));
    let st = skin::CardState {
        hovered,
        active: card.active,
        attention: card.status == Status::Attention,
    };
    // Everything of the card, backdrop included, takes the idle opacity.
    let mut back = ui.painter().clone();
    back.set_opacity(skin::card_opacity(st));
    p.set_opacity(skin::card_opacity(st));
    let tint = match card.status {
        Status::Attention => Some(theme::mix(pal.surface, pal.warn, 0.07)),
        Status::Busy => Some(theme::accent_wash(pal.surface, pal.accent_b, 0.05)),
        Status::Exited { clean: false } => Some(theme::mix(pal.surface, pal.danger, 0.07)),
        _ => None,
    };
    // Neutral and Neon paint their legacy flat card; the gallery styles go through the skin.
    if theme::is_gallery() {
        if card.active {
            skin::paint_spot(&back, rect, pal);
        }
        skin::paint_card(&back, id, rect, pal, st, tint);
        skin::paint_card_band(
            &back,
            rect,
            PAD + ROW_HEAD + ROW_PROJECT + SECTION_GAP,
            pal,
            hovered,
        );
    } else {
        let tint = tint.unwrap_or(pal.surface);
        p.rect_filled(
            rect,
            theme::rad(10, 12),
            if hovered {
                theme::mix(tint, pal.raised, 0.6)
            } else {
                tint
            },
        );
        let border = if card.active {
            pal.accent
        } else if card.status == Status::Attention {
            pal.warn.gamma_multiply(0.7)
        } else if hovered {
            theme::hover_stroke(pal)
        } else {
            pal.stroke
        };
        p.rect_stroke(
            rect,
            theme::rad(10, 12),
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );
    }
    let stripe = Rect::from_min_max(
        pos2(rect.left() + 1.0, rect.top() + 12.0),
        pos2(rect.left() + 4.0, rect.bottom() - 12.0),
    );
    if lk.grad_accent {
        skin::gradient_card(
            &p,
            id.with("stripe"),
            stripe,
            2.0,
            (color, color.gamma_multiply(0.3)),
            skin::Dir::Down,
        );
    } else {
        p.rect_filled(stripe, 2, color);
    }

    let left = rect.left() + PAD + 4.0;
    let right = rect.right() - PAD;
    let text_x = left + 24.0;
    let mut y = rect.top() + PAD;

    // Head: status glyph, title … status label.
    let cy = y + ROW_HEAD / 2.0;
    let glyph = pos2(left + 9.0, cy);
    if !paint_status(ui, glyph, 18.0, card.status.as_tab()) {
        p.circle_filled(
            glyph,
            4.0,
            if card.status == Status::Running {
                pal.accent
            } else {
                pal.weak
            },
        );
    }
    let label_left = chip_right(
        &p,
        right,
        cy,
        card.status.label(),
        color,
        color.gamma_multiply(0.16),
    );
    let title = line(
        &p,
        &card.title,
        14.5,
        pal.text_strong,
        label_left - 10.0 - text_x,
    );
    p.galley(
        pos2(text_x, cy - title.size().y / 2.0),
        title,
        pal.text_strong,
    );
    y += ROW_HEAD;

    // Project … branch, age.
    let cy = y + ROW_PROJECT / 2.0;
    paint_icon(
        &p,
        pos2(left + 9.0, cy),
        Align2::CENTER_CENTER,
        ICON_FOLDER,
        13.0,
        pal.weak,
    );
    let mut limit = right;
    if !card.age.is_empty() {
        let g = p.layout_no_wrap(card.age.clone(), theme::label_font(11.5), pal.weak);
        limit = right - g.size().x - 10.0;
        p.galley(pos2(right - g.size().x, cy - g.size().y / 2.0), g, pal.weak);
    }
    if let Some(branch) = &card.branch {
        let icon = if card.worktree {
            ICON_FOLDER_GIT_2
        } else {
            ICON_GIT_BRANCH
        };
        let g = line(&p, branch, 11.5, pal.text, (right - left) * 0.4);
        let w = g.size().x + 26.0;
        let r = Rect::from_min_size(pos2(limit - w, cy - 9.0), vec2(w, 18.0));
        skin::paint_chip(&p, r, theme::ui_rad(9), pal, pal.raised.gamma_multiply(0.8));
        paint_icon(
            &p,
            pos2(r.left() + 12.0, cy),
            Align2::CENTER_CENTER,
            icon,
            12.0,
            pal.weak,
        );
        p.galley(pos2(r.left() + 21.0, cy - g.size().y / 2.0), g, pal.text);
        limit = r.left() - 8.0;
    }
    let project = line(&p, &card.project, 12.0, pal.weak, limit - text_x);
    p.galley(pos2(text_x, cy - project.size().y / 2.0), project, pal.weak);
    y += ROW_PROJECT;

    if let Some(m) = &card.metrics {
        y += SECTION_GAP;
        // Model, thinking … cost.
        let cy = y + ROW_MODEL / 2.0;
        let mut x = left;
        if !m.model.is_empty() {
            let g = line(&p, &m.model, 11.5, pal.text, (right - left) * 0.5);
            let w = g.size().x + 14.0;
            let r = Rect::from_min_size(pos2(x, cy - 10.0), vec2(w, 20.0));
            skin::paint_chip(
                &p,
                r,
                theme::ui_rad(10),
                pal,
                pal.raised.gamma_multiply(0.9),
            );
            p.galley(pos2(x + 7.0, cy - g.size().y / 2.0), g, pal.text);
            x += w + 6.0;
        }
        if let Some(t) = &m.thinking {
            let g = p.layout_no_wrap(t.clone(), theme::label_font(11.0), pal.weak);
            paint_icon(
                &p,
                pos2(x + 7.0, cy),
                Align2::CENTER_CENTER,
                ICON_BRAIN,
                13.0,
                pal.weak,
            );
            p.galley(pos2(x + 17.0, cy - g.size().y / 2.0), g, pal.weak);
        }
        let cost = p.layout_no_wrap(m.cost.clone(), theme::font(12.0), pal.text);
        let cost_w = cost.size().x;
        p.galley(
            pos2(right - cost_w, cy - cost.size().y / 2.0),
            cost,
            pal.text,
        );
        if m.failed {
            let g = p.layout_no_wrap(
                "last turn failed".to_owned(),
                theme::label_font(11.0),
                pal.danger,
            );
            p.galley(
                pos2(right - cost_w - 10.0 - g.size().x, cy - g.size().y / 2.0),
                g,
                pal.danger,
            );
        }
        y += ROW_MODEL;

        // Context meter … tokens.
        let cy = y + ROW_CTX / 2.0;
        let bar_w = ((right - left) * 0.4).clamp(70.0, 200.0);
        let bar = Rect::from_min_size(pos2(left, cy - 3.0), vec2(bar_w, 6.0));
        p.rect_filled(bar, 3, pal.raised);
        match &m.ctx {
            Some(ctx) => {
                let col = level_color(pal, ctx.level);
                if let Some(f) = ctx.frac {
                    let w = (bar_w * f).max(if f > 0.0 { 3.0 } else { 0.0 });
                    let fill = Rect::from_min_size(bar.min, vec2(w, 6.0));
                    if theme::look().grad_accent {
                        skin::gradient_card(
                            &p,
                            id.with("meter"),
                            fill,
                            3.0,
                            (pal.accent_a, pal.accent_b),
                            skin::Dir::Right,
                        );
                    } else {
                        p.rect_filled(fill, 3, col);
                    }
                }
                let hot = ctx.level.is_some_and(|l| l != ContextLevel::Normal);
                let g = p.layout_no_wrap(
                    ctx.label.clone(),
                    theme::label_font(11.5),
                    if hot { col } else { pal.text },
                );
                p.galley(pos2(bar.right() + 8.0, cy - g.size().y / 2.0), g, pal.text);
            }
            None => {
                let g = p.layout_no_wrap("context ?".to_owned(), theme::label_font(11.5), pal.weak);
                p.galley(pos2(bar.right() + 8.0, cy - g.size().y / 2.0), g, pal.weak);
            }
        }
        let toks = p.layout_no_wrap(
            format!("↑{}  ↓{}", m.input, m.output),
            theme::label_font(11.5),
            pal.weak,
        );
        p.galley(
            pos2(right - toks.size().x, cy - toks.size().y / 2.0),
            toks,
            pal.weak,
        );
        y += ROW_CTX;
    }

    // Changed files.
    if let Some(f) = &card.files {
        let cy = y + ROW_FILES / 2.0;
        if f.is_clean() {
            paint_icon(
                &p,
                pos2(left + 7.0, cy),
                Align2::CENTER_CENTER,
                ICON_CHECK,
                13.0,
                pal.ok,
            );
            let g = p.layout_no_wrap("no changes".to_owned(), theme::label_font(11.5), pal.weak);
            p.galley(pos2(left + 18.0, cy - g.size().y / 2.0), g, pal.weak);
        } else {
            let mut job = LayoutJob::default();
            for (n, what, col) in [
                (f.staged, "staged", pal.ok),
                (f.unstaged, "modified", pal.warn),
                (f.untracked, "untracked", pal.weak),
            ] {
                if n > 0 {
                    job.append(
                        &format!("{n} {what}   "),
                        0.0,
                        egui::TextFormat {
                            font_id: theme::label_font(11.5),
                            color: col,
                            ..egui::TextFormat::default()
                        },
                    );
                }
            }
            job.wrap.max_width = right - left;
            job.wrap.max_rows = 1;
            job.wrap.overflow_character = Some('…');
            let g = p.layout_job(job);
            p.galley(pos2(left, cy - g.size().y / 2.0), g, pal.text);
        }
        y += ROW_FILES;
    }

    // Last reply.
    if let Some(reply) = card.metrics.as_ref().and_then(|m| m.last_reply.as_ref()) {
        y += SECTION_GAP;
        let h = REPLY_LINES as f32 * REPLY_LINE_H;
        p.rect_filled(
            Rect::from_min_size(pos2(left, y + 1.0), vec2(2.0, h - 2.0)),
            1,
            pal.accent.gamma_multiply(0.45),
        );
        let mut job = LayoutJob::simple(
            reply.clone(),
            theme::font(12.0),
            pal.text.gamma_multiply(0.9),
            (right - left - 10.0).max(40.0),
        );
        job.wrap.max_rows = REPLY_LINES;
        job.wrap.overflow_character = Some('…');
        let g = p.layout_job(job);
        p.with_clip_rect(Rect::from_min_size(pos2(left, y), vec2(right - left, h)))
            .galley(pos2(left + 10.0, y), g, pal.text);
        y += h;
    }

    // Subagents.
    let mut toggled = None;
    if let Some(sa) = sa
        && sa.view.counts.total() > 0
    {
        y += SECTION_GAP;
        let counts = sa.view.counts;
        let g = acc::geo(PAD);
        let lk = theme::look().acc;
        let header = if g.edge {
            Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), g.h))
        } else {
            Rect::from_min_max(
                pos2(rect.left() + g.box_l, y),
                pos2(rect.right() - g.box_r, y + g.h),
            )
        };
        let r = ui.interact(header, id.with("agents"), Sense::click());
        let pill_w = if lk.kind == theme::Acc::Pill {
            230.0
        } else {
            header.width()
        };
        let slots = if lk.kind == theme::Acc::Row {
            if r.hovered() || c.open {
                p.rect_filled(
                    header,
                    theme::ui_rad(8),
                    pal.raised
                        .gamma_multiply(if r.hovered() { 0.9 } else { 0.5 }),
                );
            }
            let cy = header.center().y;
            paint_icon(
                &p,
                pos2(header.left() + 12.0, cy),
                Align2::CENTER_CENTER,
                if c.open {
                    ICON_CHEVRON_DOWN
                } else {
                    ICON_CHEVRON_RIGHT
                },
                15.0,
                pal.weak,
            );
            acc::Slots {
                text_x: header.left() + 26.0,
                right: header.right(),
            }
        } else {
            acc::paint_back(
                &back,
                id.with("acc"),
                pal,
                &acc::Frame {
                    card: rect,
                    header,
                    bottom: rect.bottom() - g.foot,
                    open: c.open,
                    hovered: r.hovered(),
                    card_hovered: hovered,
                    pill_w,
                },
            )
        };
        let cy = header.center().y;
        let mut x = slots.text_x;
        if counts.running > 0 {
            Spin::new(14.0, pal.accent_b).paint_at(ui, pos2(x + 7.0, cy));
            x += 20.0;
        }
        let summary = if counts.running > 0 {
            format!("{} running of {} subagents", counts.running, counts.total())
        } else if counts.total() == 1 {
            "1 subagent".to_owned()
        } else {
            format!("{} subagents", counts.total())
        };
        let ink = if theme::style() == UiStyle::Phosphor {
            pal.accent
        } else if lk.strong {
            pal.text_strong
        } else {
            pal.text
        };
        let galley = if lk.mono_header {
            let mut job = LayoutJob::default();
            job.append(
                &summary.to_uppercase(),
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::new(10.5, egui::FontFamily::Monospace),
                    color: ink,
                    extra_letter_spacing: 0.8,
                    ..egui::TextFormat::default()
                },
            );
            p.layout_job(job)
        } else {
            p.layout_no_wrap(summary, theme::font(12.0), ink)
        };
        let mut rx = x + galley.size().x + 12.0;
        p.galley(pos2(x, cy - galley.size().y / 2.0), galley, ink);
        for (n, col, what) in [
            (counts.failed, pal.danger, "failed"),
            (counts.aborted + counts.interrupted, pal.warn, "stopped"),
        ] {
            if n > 0 {
                let g = p.layout_no_wrap(format!("{n} {what}"), theme::label_font(11.5), col);
                let w = g.size().x;
                p.galley(pos2(rx, cy - g.size().y / 2.0), g, col);
                rx += w + 10.0;
            }
        }
        if lk.kind != theme::Acc::Row {
            let hint = p.layout_no_wrap(
                if c.open { "Hide" } else { "Show" }.to_owned(),
                theme::label_font(11.5),
                pal.weak,
            );
            let hx = slots.right - hint.size().x;
            let mut fill_end = hx - 8.0;
            if lk.segbar {
                let segs = [
                    (counts.running, pal.accent),
                    (counts.done, pal.ok),
                    (counts.failed, pal.danger),
                    (counts.aborted + counts.interrupted, pal.warn),
                    (counts.parked, pal.weak),
                ];
                let bar = Rect::from_min_size(pos2(hx - 8.0 - 64.0, cy - 3.0), vec2(64.0, 6.0));
                acc::paint_segbar(&p, bar, &segs);
                fill_end = bar.left() - 8.0;
            }
            acc::paint_dashed_fill(&p, pal, header, rx + 4.0, fill_end);
            p.galley(pos2(hx, cy - hint.size().y / 2.0), hint, pal.weak);
        }
        if r.on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(if c.open {
                "Hide the subagents"
            } else {
                "Show the subagents"
            })
            .clicked()
        {
            toggled = Some(card.key.clone());
        }
        y += g.h;

        if c.open {
            y += g.pad_top;
            if let Some(text) = c.note {
                let r = Rect::from_min_size(
                    pos2(rect.left() + g.rows_l, y),
                    vec2(rect.width() - g.rows_l - g.rows_r, NOTE_H),
                );
                if note_strip(ui, r, text, c.ctl) {
                    out.push(DashAction::Restart);
                }
                y += NOTE_H + SECTION_GAP;
            }
            let inner = Rect::from_min_max(
                pos2(rect.left() + g.rows_l, y),
                pos2(rect.right() - g.rows_r, rect.bottom() - g.foot),
            );
            if lk.rail != theme::Rail::None {
                acc::paint_rail(
                    &back,
                    id.with("rail"),
                    pal,
                    rect.left() + PAD + 12.0,
                    y,
                    rect.bottom() - g.foot - g.pad_bottom,
                );
            }
            let session = card.session.clone().unwrap_or_default();
            let selected = c
                .popup
                .filter(|(s, _)| *s == session.as_path())
                .map(|(_, id)| id);
            ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
                ui.push_id(&card.key, |ui| {
                    let mut rows = Vec::new();
                    let input = RowsInput {
                        view: &sa.view,
                        tree: c.tree,
                        selected,
                        ctl: c.ctl,
                    };
                    agent_hub::rows(ui, &mut sa.rows, &input, inner.width(), &mut rows);
                    for a in rows {
                        out.push(match a {
                            RowAction::Open(id) => DashAction::Agent {
                                session: session.clone(),
                                id,
                            },
                            RowAction::Kill(id) => DashAction::Kill(id),
                            RowAction::Revive(id) => DashAction::Revive(id),
                        });
                    }
                });
            });
        }
    }

    if resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_ui(|ui| {
            ui.label(RichText::new(&card.title).strong());
            ui.label(format!("id {}", card.id));
            ui.label(&card.project);
            if let Some(s) = &card.session {
                ui.label(s.display().to_string());
            }
        })
        .clicked()
    {
        out.push(DashAction::OpenCard(card.key.clone()));
    }
    toggled
}

/// The one-line live-control note of the active card; true when "Restart" was pressed.
fn note_strip(ui: &mut Ui, rect: Rect, text: &str, ctl: &CtlView<'_>) -> bool {
    let pal = Palette::of_ui(ui);
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 8, theme::mix(pal.surface, pal.warn, 0.08));
    p.rect_stroke(
        rect,
        8,
        Stroke::new(1.0, pal.warn.gamma_multiply(0.35)),
        StrokeKind::Inside,
    );
    let mut pressed = false;
    let mut limit = rect.right() - 10.0;
    if ctl.restart != Restart::Hidden {
        let ready = ctl.restart == Restart::Ready;
        let br = Rect::from_min_size(
            pos2(rect.right() - 10.0 - 190.0, rect.center().y - 11.0),
            vec2(190.0, 22.0),
        );
        let button = ui
            .put(
                br,
                egui::Button::new(RichText::new("Restart with live control").size(11.5)),
            )
            .on_hover_text(if ready {
                "Stops this omp process and resumes the same session here, with the control \
                 extension loaded"
            } else {
                "omp is busy: restart once it is idle"
            });
        pressed = button.clicked() && ready;
        limit = br.left() - 8.0;
    }
    let g = line(&p, text, 11.5, pal.weak, limit - rect.left() - 12.0);
    p.galley(
        pos2(rect.left() + 12.0, rect.center().y - g.size().y / 2.0),
        g,
        pal.weak,
    );
    pressed
}
