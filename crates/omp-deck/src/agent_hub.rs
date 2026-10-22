//! The native Agent Hub side panel: aggregate header, search, status filters, tree/flat roster of
//! subagent cards. Cards are painted directly and culled to the viewport; their heights are a
//! pure function of the data, so the scroll range is exact without measuring.

use std::collections::HashSet;
use std::sync::Arc;

use deck_core::agents::{AgentStatus, AgentSummary};
use deck_core::ctl::LiveStatus;
use egui::Galley;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, CursorIcon, FontFamily, FontId, Frame, Id, Margin, Rect, RichText, ScrollArea,
    Sense, Sides, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2,
};
use egui_material_icons::MaterialIcon;
use egui_material_icons::icons::{
    ICON_ACCOUNT_TREE, ICON_BEDTIME, ICON_CHECK_CIRCLE, ICON_CHEVRON_RIGHT, ICON_CLOSE, ICON_ERROR,
    ICON_EXPAND_MORE, ICON_HUB, ICON_PAUSE_CIRCLE, ICON_PLAY_ARROW, ICON_SEARCH, ICON_STOP,
    ICON_STOP_CIRCLE, ICON_SYNC, ICON_VIEW_LIST, ICON_WARNING,
};

use crate::agent_ctl::{self, CtlView, Link, Restart};
use crate::agent_model::{self, AgentsView, BUCKETS, Bucket, CardView, Filter, status_label};
use crate::theme::{self, Palette};
use crate::widgets::{Spin, empty_state, icon_text, paint_icon};

const CARD_PAD: f32 = 11.0;
const CARD_GAP: f32 = 6.0;
const INDENT: f32 = 14.0;
const OVERSCAN: f32 = 300.0;

pub fn status_color(pal: &Palette, s: AgentStatus) -> Color32 {
    match s {
        AgentStatus::Running => pal.accent_b,
        AgentStatus::Done => pal.ok,
        AgentStatus::Failed => pal.danger,
        AgentStatus::Aborted => pal.warn,
        AgentStatus::Interrupted => theme::mix(pal.warn, pal.weak, 0.45),
    }
}

fn bucket_icon(b: Bucket) -> MaterialIcon {
    match b {
        Bucket::Running => status_icon(AgentStatus::Running),
        Bucket::Done => status_icon(AgentStatus::Done),
        Bucket::Failed => status_icon(AgentStatus::Failed),
        Bucket::Aborted => status_icon(AgentStatus::Aborted),
        Bucket::Interrupted => status_icon(AgentStatus::Interrupted),
        Bucket::Parked => ICON_PAUSE_CIRCLE,
    }
}

/// Chip colour of a bucket; parked agents use the same hue as their card state.
fn bucket_color(pal: &Palette, b: Bucket) -> Color32 {
    match b {
        Bucket::Running => status_color(pal, AgentStatus::Running),
        Bucket::Done => status_color(pal, AgentStatus::Done),
        Bucket::Failed => status_color(pal, AgentStatus::Failed),
        Bucket::Aborted => status_color(pal, AgentStatus::Aborted),
        Bucket::Interrupted => status_color(pal, AgentStatus::Interrupted),
        Bucket::Parked => theme::mix(pal.weak, pal.accent, 0.4),
    }
}

pub fn status_icon(s: AgentStatus) -> MaterialIcon {
    match s {
        AgentStatus::Running => ICON_SYNC,
        AgentStatus::Done => ICON_CHECK_CIRCLE,
        AgentStatus::Failed => ICON_ERROR,
        AgentStatus::Aborted => ICON_STOP_CIRCLE,
        AgentStatus::Interrupted => ICON_WARNING,
    }
}

/// What one agent shows as its state: omp's live word where it knows better than the files
/// (`parked`, `idle`), else the file-inferred status.
pub struct Shown {
    pub label: &'static str,
    pub icon: MaterialIcon,
    pub color: Color32,
    pub spin: bool,
}

pub fn shown(pal: &Palette, status: AgentStatus, live: Option<LiveStatus>) -> Shown {
    match live {
        Some(LiveStatus::Parked) => Shown {
            label: "parked",
            icon: ICON_PAUSE_CIRCLE,
            color: theme::mix(pal.weak, pal.accent, 0.4),
            spin: false,
        },
        Some(LiveStatus::Idle) if status == AgentStatus::Done => Shown {
            label: "idle",
            icon: ICON_BEDTIME,
            color: pal.ok,
            spin: false,
        },
        _ => Shown {
            label: status_label(status),
            icon: status_icon(status),
            color: status_color(pal, status),
            spin: status == AgentStatus::Running,
        },
    }
}

/// Panel state owned by the app.
pub struct HubState {
    pub filter: Filter,
    collapsed: HashSet<String>,
    collapsed_gen: u64,
    order: Vec<usize>,
    order_key: Option<(u64, Filter, u64)>,
    offsets: Vec<f32>,
    /// Measured gist galleys by snapshot index; valid for `gist_key`.
    gists: Vec<Option<Arc<Galley>>>,
    gist_key: Option<(u64, i32, bool, bool)>,
}

impl Default for HubState {
    fn default() -> Self {
        HubState {
            filter: Filter {
                tree: true,
                ..Filter::default()
            },
            collapsed: HashSet::new(),
            collapsed_gen: 0,
            order: Vec::new(),
            order_key: None,
            offsets: Vec::new(),
            gists: Vec::new(),
            gist_key: None,
        }
    }
}

impl HubState {
    /// Forget the per-session view state (query, status filter, collapsed groups); the tree/flat
    /// choice is a preference and stays.
    pub fn reset(&mut self) {
        let tree = self.filter.tree;
        *self = HubState::default();
        self.filter.tree = tree;
    }

    /// Visible agent indices (into the view's snapshot) in display order.
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    fn refresh_order(&mut self, view: &AgentsView) {
        let key = (view.version, self.filter.clone(), self.collapsed_gen);
        if self.order_key.as_ref() == Some(&key) {
            return;
        }
        self.order = agent_model::visible(
            view.agents(),
            &view.search,
            &self.filter,
            &self.collapsed,
            |i| {
                view.cards
                    .get(i)
                    .is_some_and(|c| c.live == Some(LiveStatus::Parked))
            },
        );
        self.order_key = Some(key);
    }

    /// Recompute the order for the current view (the popup navigates it too).
    pub fn sync(&mut self, view: &AgentsView) {
        self.refresh_order(view);
    }
}

pub enum HubAction {
    Open(String),
    Close,
    /// Ask to kill this agent (the app confirms first).
    Kill(String),
    Revive(String),
    /// Ask to restart the session with live control (the app confirms first).
    Restart,
}

pub struct HubInput<'a> {
    pub view: &'a AgentsView,
    /// The active tab has a session file to watch.
    pub has_session: bool,
    /// Agent whose popup is open (highlighted).
    pub selected: Option<&'a str>,
    pub ctl: &'a CtlView<'a>,
}

/// Draws the panel contents.
pub fn show(ui: &mut Ui, st: &mut HubState, input: &HubInput<'_>) -> Vec<HubAction> {
    let pal = Palette::of_ui(ui);
    let mut out = Vec::new();
    st.refresh_order(input.view);
    let view = input.view;

    // Title row.
    Sides::new().height(30.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(icon_text(ICON_HUB, 20.0, pal.accent));
            ui.label(theme::strong("Agents").size(16.0).color(pal.text_strong));
            let total = view.counts.total();
            if total > 0 {
                pill(ui, &total.to_string(), pal.weak, pal.raised);
            }
            link_badge(ui, input.ctl.link);
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if small_button(ui, ICON_CLOSE, "Close the hub (Ctrl+Shift+A)", false).clicked() {
                out.push(HubAction::Close);
            }
            ui.add_space(4.0);
            if small_button(
                ui,
                ICON_VIEW_LIST,
                "Flat list: running first, newest activity",
                !st.filter.tree,
            )
            .clicked()
            {
                st.filter.tree = false;
            }
            if small_button(
                ui,
                ICON_ACCOUNT_TREE,
                "Tree: who spawned whom",
                st.filter.tree,
            )
            .clicked()
            {
                st.filter.tree = true;
            }
        },
    );
    ui.add_space(8.0);

    if !input.has_session {
        empty_state(
            ui,
            ICON_HUB,
            pal.accent,
            "No session in this tab",
            "Open or start an omp session to see the agents it spawns.",
        );
        return out;
    }
    link_note(ui, input.ctl, &mut out);

    let total = view.counts.total();
    if total > 0 {
        // Aggregate = filter chips.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            for s in BUCKETS {
                let n = view.counts.get(s);
                let on = st.filter.status.contains(s);
                if n == 0 && !on {
                    continue;
                }
                if status_chip(ui, s, n, on).clicked() {
                    st.filter.status.toggle(s);
                }
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new(&view.totals).size(11.5).color(pal.weak));
        ui.add_space(8.0);
        search_box(ui, st);
        ui.add_space(10.0);
    }

    match view.snapshot() {
        None => {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.add(Spin::new(22.0, pal.accent));
                ui.add_space(6.0);
                ui.label(RichText::new("Looking for agents…").color(pal.weak));
            });
        }
        Some(_) if total == 0 => empty_state(
            ui,
            ICON_HUB,
            pal.accent,
            "No subagents yet",
            "Agents this session spawns with the task tool show up here, live.",
        ),
        Some(_) if st.order.is_empty() => {
            empty_state(
                ui,
                ICON_SEARCH,
                pal.weak,
                "Nothing matches",
                "Try another search or clear the status filters.",
            );
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                if ui.button("Clear filters").clicked() {
                    st.filter.query.clear();
                    st.filter.status = agent_model::StatusFilter::default();
                }
            });
        }
        Some(_) => roster(ui, st, input, &mut out),
    }
    out
}

fn search_box(ui: &mut Ui, st: &mut HubState) {
    let pal = Palette::of_ui(ui);
    let id = Id::unique("agent-hub-search");
    let focused = ui.memory(|m| m.has_focus(id));
    ui.add(
        TextEdit::singleline(&mut st.filter.query)
            .id(id)
            .prefix(icon_text(ICON_SEARCH, 16.0, pal.weak))
            .hint_text("Search id, agent, task…")
            .align(Align2::LEFT_CENTER)
            .min_size(vec2(0.0, 32.0))
            .desired_width(ui.available_width())
            .margin(Margin::symmetric(10, 6))
            .frame(
                Frame::new()
                    .fill(pal.surface)
                    .corner_radius(10)
                    .stroke(Stroke::new(
                        1.0,
                        if focused { pal.accent } else { pal.stroke },
                    )),
            ),
    );
}

fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(11.5), fg);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 18.0), Sense::hover());
    ui.painter().rect_filled(rect, 9, bg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
}

fn small_button(ui: &mut Ui, icon: MaterialIcon, tip: &str, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() || on, 0.1);
    let p = ui.painter();
    p.rect_filled(
        rect,
        8,
        if on {
            pal.accent.gamma_multiply(0.28)
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

fn status_chip(ui: &mut Ui, b: Bucket, n: usize, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let color = bucket_color(pal, b);
    let text = format!("{n} {}", b.label());
    let g = ui.painter().layout_no_wrap(
        text,
        FontId::proportional(12.0),
        if on { pal.text_strong } else { pal.text },
    );
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 38.0, 24.0), Sense::click());
    let hov = resp.hovered();
    let p = ui.painter();
    p.rect_filled(
        rect,
        12,
        if on {
            color.gamma_multiply(0.22)
        } else if hov {
            pal.raised
        } else {
            pal.surface
        },
    );
    p.rect_stroke(
        rect,
        12,
        Stroke::new(1.0, if on { color } else { pal.stroke }),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        pos2(rect.left() + 14.0, rect.center().y),
        Align2::CENTER_CENTER,
        bucket_icon(b),
        14.0,
        color,
    );
    p.galley(
        pos2(rect.left() + 26.0, rect.center().y - g.size().y / 2.0),
        g,
        pal.text,
    );
    resp.on_hover_text(if on {
        "Click to stop filtering by this status"
    } else {
        "Click to show only this status"
    })
    .on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ roster

fn error_line(a: &AgentSummary) -> Option<&str> {
    match a.status {
        AgentStatus::Failed | AgentStatus::Aborted | AgentStatus::Interrupted => {
            a.error.as_deref().filter(|e| !e.trim().is_empty())
        }
        _ => None,
    }
}

fn current_line(a: &AgentSummary) -> Option<&str> {
    (a.status == AgentStatus::Running)
        .then_some(a.current.as_deref())
        .flatten()
        .filter(|c| !c.is_empty())
}

/// Wrap width of the gist inside a card at `depth` for a panel `width` wide.
fn gist_wrap_width(width: f32, depth: usize) -> f32 {
    (width - depth as f32 * INDENT - 2.0 - 2.0 * CARD_PAD - 6.0).max(40.0)
}

/// Lays out the gist exactly as it is painted (two rows at most).
fn layout_gist(ui: &Ui, pal: &Palette, gist: &str, wrap_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple(
        gist.to_owned(),
        FontId::proportional(12.5),
        pal.text.gamma_multiply(0.92),
        wrap_w,
    );
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    ui.painter().layout_job(job)
}

fn card_height(a: &AgentSummary, gist: Option<&Arc<Galley>>) -> f32 {
    let mut h = CARD_PAD * 2.0 + 22.0 + 18.0;
    if let Some(g) = gist {
        h += 4.0 + g.size().y.ceil();
    }
    if current_line(a).is_some() {
        h += 20.0;
    }
    if error_line(a).is_some() {
        h += 20.0;
    }
    h
}

fn roster(ui: &mut Ui, st: &mut HubState, input: &HubInput<'_>, out: &mut Vec<HubAction>) {
    let pal = Palette::of_ui(ui);
    let view = input.view;
    let agents = view.agents();
    let width = ui.available_width();
    let tree = st.filter.tree;
    // Gist galleys are measured once per (snapshot, panel width, layout mode, theme).
    let gist_key = (
        view.base_version,
        width.round() as i32,
        tree,
        ui.visuals().dark_mode,
    );
    if st.gist_key != Some(gist_key) {
        st.gists.clear();
        st.gists.resize(agents.len(), None);
        st.gist_key = Some(gist_key);
    }
    // Row geometry from the measured galleys.
    st.offsets.clear();
    st.offsets.push(0.0);
    let mut y = 0.0;
    for k in 0..st.order.len() {
        let i = st.order[k];
        let gist = &view.cards[i].gist;
        if !gist.is_empty() && st.gists[i].is_none() {
            let depth = if tree { agents[i].depth.min(6) } else { 0 };
            st.gists[i] = Some(layout_gist(ui, pal, gist, gist_wrap_width(width, depth)));
        }
        y += card_height(&agents[i], st.gists[i].as_ref()) + CARD_GAP;
        st.offsets.push(y);
    }
    let total = y;
    let n = st.order.len();
    let mut toggled = None;
    ScrollArea::vertical()
        .id_salt("agent-hub-roster")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let lo = viewport.min.y - OVERSCAN;
            let hi = viewport.max.y + OVERSCAN;
            let first = st.offsets[1..].partition_point(|&o| o <= lo).min(n);
            let end = st.offsets[..n].partition_point(|&o| o < hi).max(first);
            ui.add_space(st.offsets[first]);
            for r in first..end {
                let i = st.order[r];
                let h = st.offsets[r + 1] - st.offsets[r];
                let a = &agents[i];
                let c = &view.cards[i];
                let depth = if tree { a.depth.min(6) } else { 0 };
                let (row, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
                if !ui.is_rect_visible(row) {
                    continue;
                }
                let sel = input.selected == Some(a.id.as_str());
                let tree_depth = tree.then_some(depth);
                let opts = CardOpts {
                    tree_depth,
                    selected: sel,
                    act: card_act(input.ctl, a, c),
                };
                match card(ui, row, a, c, st.gists[i].as_ref(), opts) {
                    CardClick::Open => out.push(HubAction::Open(a.id.clone())),
                    CardClick::ToggleChildren => toggled = Some(a.id.clone()),
                    CardClick::Act(CardAct::Kill) => out.push(HubAction::Kill(a.id.clone())),
                    CardClick::Act(CardAct::Revive) => out.push(HubAction::Revive(a.id.clone())),
                    CardClick::None => {}
                }
            }
            ui.add_space((total - st.offsets[end]).max(0.0));
        });
    if let Some(id) = toggled {
        if !st.collapsed.remove(&id) {
            st.collapsed.insert(id);
        }
        st.collapsed_gen += 1;
    }
}

enum CardClick {
    None,
    Open,
    ToggleChildren,
    Act(CardAct),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CardAct {
    Kill,
    Revive,
}

struct CardOpts {
    /// `Some(depth)` in tree mode, `None` in the flat list.
    tree_depth: Option<usize>,
    selected: bool,
    act: Option<CardAct>,
}

fn card_act(ctl: &CtlView<'_>, a: &AgentSummary, c: &CardView) -> Option<CardAct> {
    if ctl.pending(&a.id).is_some() {
        None
    } else if agent_ctl::can_kill(ctl.link, c.live) {
        Some(CardAct::Kill)
    } else if agent_ctl::can_revive(ctl.link, c.live) {
        Some(CardAct::Revive)
    } else {
        None
    }
}

fn card(
    ui: &mut Ui,
    row: Rect,
    a: &AgentSummary,
    c: &CardView,
    gist: Option<&Arc<Galley>>,
    opts: CardOpts,
) -> CardClick {
    let CardOpts {
        tree_depth,
        selected,
        act,
    } = opts;
    let (tree, depth) = (tree_depth.is_some(), tree_depth.unwrap_or(0));
    let pal = Palette::of_ui(ui);
    let sh = shown(pal, a.status, c.live);
    let color = sh.color;
    let rect = Rect::from_min_max(
        pos2(row.left() + depth as f32 * INDENT, row.top()),
        pos2(row.right() - 2.0, row.bottom() - CARD_GAP),
    );
    let id = ui.scope_id().with(("agent-card", &a.id));
    let resp = ui.interact(rect, id, Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter_at(row);

    // Guide lines: one per ancestor level, continuous across rows.
    for lvl in 0..depth {
        let x = row.left() + lvl as f32 * INDENT + 6.0;
        p.vline(
            x,
            row.y_range(),
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
        );
    }
    if depth > 0 {
        let x = row.left() + (depth - 1) as f32 * INDENT + 6.0;
        p.hline(
            x..=rect.left(),
            rect.top() + 21.0,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
        );
    }

    let tint = match a.status {
        AgentStatus::Running => theme::mix(pal.surface, pal.accent_b, 0.06),
        AgentStatus::Failed => theme::mix(pal.surface, pal.danger, 0.07),
        AgentStatus::Aborted | AgentStatus::Interrupted => theme::mix(pal.surface, pal.warn, 0.05),
        AgentStatus::Done => pal.surface,
    };
    let fill = if hovered {
        theme::mix(tint, pal.raised, 0.7)
    } else {
        tint
    };
    p.rect_filled(rect, 11, fill);
    let border = if selected {
        pal.accent
    } else if hovered {
        pal.accent.gamma_multiply(0.55)
    } else {
        pal.stroke
    };
    p.rect_stroke(rect, 11, Stroke::new(1.0, border), StrokeKind::Inside);
    p.rect_filled(
        Rect::from_min_max(
            pos2(rect.left() + 1.0, rect.top() + 10.0),
            pos2(rect.left() + 4.0, rect.bottom() - 10.0),
        ),
        2,
        color,
    );

    let left = rect.left() + CARD_PAD + 6.0;
    let right = rect.right() - CARD_PAD;
    let mut y = rect.top() + CARD_PAD;

    // Row 1: glyph, id, badges … model.
    let mid = y + 11.0;
    if sh.spin {
        Spin::new(16.0, color).paint_at(ui, pos2(left + 8.0, mid));
    } else {
        paint_icon(
            &p,
            pos2(left + 8.0, mid),
            Align2::CENTER_CENTER,
            sh.icon,
            17.0,
            color,
        );
    }
    let mut x = left + 24.0;
    let id_g = p.layout_no_wrap(a.id.clone(), FontId::proportional(13.5), pal.text_strong);
    let id_w = id_g.size().x;
    // Model chip on the right (dropped when it would crowd the id).
    let model_g = (!c.short_model.is_empty())
        .then(|| p.layout_no_wrap(c.short_model.clone(), FontId::proportional(11.0), pal.weak));
    let mut limit = right;
    if let Some(g) = &model_g
        && x + id_w + 24.0 + g.size().x + 14.0 < right
    {
        let r = Rect::from_min_size(
            pos2(right - g.size().x - 14.0, mid - 9.0),
            vec2(g.size().x + 14.0, 18.0),
        );
        p.rect_filled(r, 9, pal.raised.gamma_multiply(0.8));
        p.galley(
            pos2(r.left() + 7.0, mid - g.size().y / 2.0),
            g.clone(),
            pal.weak,
        );
        limit = r.left() - 6.0;
    }
    let id_galley = if x + id_w > limit {
        // Ellipsize a very long id.
        let mut job =
            LayoutJob::simple_singleline(a.id.clone(), FontId::proportional(13.5), pal.text_strong);
        job.wrap.max_width = (limit - x).max(30.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        p.layout_job(job)
    } else {
        id_g
    };
    let id_end = x + id_galley.size().x;
    p.galley(
        pos2(x, mid - id_galley.size().y / 2.0),
        id_galley,
        pal.text_strong,
    );
    x = id_end + 8.0;
    let badges: [(Option<&str>, Color32); 3] = [
        (
            (sh.label != status_label(a.status)).then_some(sh.label),
            color,
        ),
        (a.agent.as_deref(), pal.accent),
        (a.advisor.then_some("advisor"), pal.warn),
    ];
    for (text, col) in badges {
        let Some(text) = text else { continue };
        let g = p.layout_no_wrap(text.to_owned(), FontId::proportional(10.5), col);
        let w = g.size().x + 12.0;
        if x + w > limit {
            break;
        }
        let r = Rect::from_min_size(pos2(x, mid - 8.0), vec2(w, 16.0));
        p.rect_filled(r, 8, col.gamma_multiply(0.16));
        p.galley(pos2(x + 6.0, mid - g.size().y / 2.0), g, col);
        x += w + 6.0;
    }
    y += 22.0;

    // Gist.
    let text_w = right - left;
    if let Some(g) = gist {
        y += 4.0;
        let h = g.size().y.ceil();
        p.galley(pos2(left, y), Arc::clone(g), pal.text);
        y += h;
    }

    // Current tool (running) / error.
    if let Some(cur) = current_line(a) {
        y += 4.0;
        paint_icon(
            &p,
            pos2(left + 6.0, y + 8.0),
            Align2::CENTER_CENTER,
            ICON_CHEVRON_RIGHT,
            15.0,
            pal.accent_b,
        );
        let mut job = LayoutJob::default();
        job.append(
            cur,
            0.0,
            TextFormat {
                font_id: FontId::new(11.5, FontFamily::Monospace),
                color: pal.accent_b,
                ..TextFormat::default()
            },
        );
        job.wrap.max_width = text_w - 18.0;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let g = p.layout_job(job);
        p.galley(
            pos2(left + 16.0, y + 8.0 - g.size().y / 2.0),
            g,
            pal.accent_b,
        );
        y += 16.0;
    }
    if let Some(err) = error_line(a) {
        y += 4.0;
        let mut job = LayoutJob::default();
        job.append(
            err,
            0.0,
            TextFormat {
                font_id: FontId::proportional(11.5),
                color,
                ..TextFormat::default()
            },
        );
        job.wrap.max_width = text_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let g = p.layout_job(job);
        p.galley(pos2(left, y), g, color);
    }

    // Metrics row (bottom-anchored).
    let my = rect.bottom() - CARD_PAD - 8.0;
    let age_g = p.layout_no_wrap(c.age.clone(), FontId::proportional(11.5), pal.weak);
    let mut rx = right;
    p.galley(
        pos2(rx - age_g.size().x, my - age_g.size().y / 2.0),
        age_g.clone(),
        pal.weak,
    );
    rx -= age_g.size().x + 10.0;
    let mut click = CardClick::None;
    if let Some(act) = act {
        let r = Rect::from_center_size(pos2(rx - 11.0, my), vec2(22.0, 22.0));
        let b = ui.interact(r, id.with("act"), Sense::click());
        let (icon, tip, col) = match act {
            CardAct::Kill => (ICON_STOP, "Kill this agent…", pal.danger),
            CardAct::Revive => (ICON_PLAY_ARROW, "Revive this agent", pal.ok),
        };
        let strong = b.hovered() || hovered;
        if b.hovered() {
            p.rect_filled(r, 11, col.gamma_multiply(0.22));
        }
        paint_icon(
            &p,
            r.center(),
            Align2::CENTER_CENTER,
            icon,
            16.0,
            if strong { col } else { col.gamma_multiply(0.5) },
        );
        if b.on_hover_text(tip)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            click = CardClick::Act(act);
        }
        rx = r.left() - 8.0;
    }
    if tree && c.children > 0 {
        let label = c.children.to_string();
        let g = p.layout_no_wrap(label, FontId::proportional(11.5), pal.text);
        let w = g.size().x + 30.0;
        let r = Rect::from_min_size(pos2(rx - w, my - 10.0), vec2(w, 20.0));
        let kid = ui.interact(r, id.with("kids"), Sense::click());
        p.rect_filled(
            r,
            10,
            if kid.hovered() {
                pal.raised
            } else {
                pal.raised.gamma_multiply(0.5)
            },
        );
        paint_icon(
            &p,
            pos2(r.left() + 9.0, my),
            Align2::CENTER_CENTER,
            ICON_EXPAND_MORE,
            14.0,
            pal.weak,
        );
        p.galley(pos2(r.left() + 19.0, my - g.size().y / 2.0), g, pal.text);
        if kid
            .on_hover_text("Collapse / expand children")
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            click = CardClick::ToggleChildren;
        }
        rx = r.left() - 10.0;
    }
    let mut job =
        LayoutJob::simple_singleline(c.metrics.clone(), FontId::proportional(11.5), pal.weak);
    job.wrap.max_width = (rx - left).max(30.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let g = p.layout_job(job);
    p.galley(pos2(left, my - g.size().y / 2.0), g, pal.weak);

    if act.is_some() {
        resp.context_menu(|ui| {
            if let Some(act) = act {
                let label = match act {
                    CardAct::Kill => "Kill agent…",
                    CardAct::Revive => "Revive agent",
                };
                if ui.button(label).clicked() {
                    click = CardClick::Act(act);
                    ui.close();
                }
            }
        });
    }
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() && matches!(click, CardClick::None)
    {
        click = CardClick::Open;
    }
    click
}

/// Small state pill next to the title: `Live`, `Connecting`, `Read-only`.
fn link_badge(ui: &mut Ui, link: Link) {
    let pal = Palette::of_ui(ui);
    let (text, col, tip) = match link {
        Link::Live => (
            "Live",
            pal.ok,
            "Connected to omp: kill, chat and revive are available",
        ),
        Link::Connecting => (
            "Connecting",
            pal.accent,
            "Waiting for omp's control extension to come up…",
        ),
        Link::ReadOnly => (
            "Read-only",
            pal.warn,
            "No live control: statuses are inferred from session files",
        ),
        Link::Lost => (
            "Disconnected",
            pal.warn,
            "The connection to omp's control extension was lost; statuses are inferred from session files",
        ),
        Link::Ended => (
            "Ended",
            pal.weak,
            "This session is no longer running; statuses are inferred from session files",
        ),
    };
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(11.5), col);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 26.0, 18.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 9, col.gamma_multiply(0.16));
    p.circle_filled(pos2(rect.left() + 10.0, rect.center().y), 3.0, col);
    p.galley(
        pos2(rect.left() + 18.0, rect.center().y - g.size().y / 2.0),
        g,
        col,
    );
    resp.on_hover_text(tip);
}

/// Why the hub is read-only, and the way out for tabs that can be restarted.
fn link_note(ui: &mut Ui, ctl: &CtlView<'_>, out: &mut Vec<HubAction>) {
    let note = match ctl.link {
        Link::ReadOnly => {
            "Statuses come from session files. This tab was started without live \
             control, so agents cannot be killed, messaged or revived from here."
        }
        Link::Lost => {
            "Statuses come from session files. Live control disconnected; it returns by \
             itself if omp opens its control socket again."
        }
        Link::Ended => {
            "Statuses come from session files. This session is no longer running (omp \
             exited or its tmux server went away)."
        }
        Link::Live | Link::Connecting => return,
    };
    let pal = Palette::of_ui(ui);
    Frame::new()
        .fill(theme::mix(pal.surface, pal.warn, 0.07))
        .stroke(Stroke::new(1.0, pal.warn.gamma_multiply(0.35)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(note).size(11.5).color(pal.weak));
            if ctl.restart != Restart::Hidden {
                ui.add_space(6.0);
                let ready = ctl.restart == Restart::Ready;
                let resp = ui.add_enabled(
                    ready,
                    egui::Button::new("Restart session with live control"),
                );
                let resp = if ready {
                    resp.on_hover_text(
                        "Stops this omp process and resumes the same session here, \
                         with the control extension loaded",
                    )
                } else {
                    resp.on_disabled_hover_text("omp is busy: restart once it is idle")
                };
                if resp.clicked() {
                    out.push(HubAction::Restart);
                }
            }
        });
    ui.add_space(8.0);
}
