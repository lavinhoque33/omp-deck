//! Subagent rows of the agents dashboard: the tree/flat roster of one session's subagent cards.
//! Cards are painted directly and culled to the viewport; their heights are a pure function of
//! the data (the gist galleys are measured once per snapshot and width), so the card height of
//! an expanded session is exact without measuring while painting.

use std::collections::HashSet;
use std::sync::Arc;

use crate::icons::Icon;
use crate::icons::{
    ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_CIRCLE_PAUSE,
    ICON_CIRCLE_STOP, ICON_MOON_STAR, ICON_PLAY, ICON_REFRESH_CW, ICON_SQUARE, ICON_TRIANGLE_ALERT,
};
use deck_core::agents::{AgentStatus, AgentSummary};
use deck_core::ctl::LiveStatus;
use egui::Galley;
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, CursorIcon, FontFamily, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::agent_ctl::{self, CtlView};
use crate::agent_model::{self, AgentsView, CardView, MetricKind, status_label};
use crate::badge::{Badge, Hue};
use crate::icons::ICON_CPU;
use crate::theme::{self, Palette};
use crate::widgets::{Spin, paint_icon};

const CARD_PAD: f32 = 11.0;
/// Space between two rows (none where rows are separated by rules).
fn card_gap() -> f32 {
    theme::acc::row_gap()
}
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

pub fn status_icon(s: AgentStatus) -> Icon {
    match s {
        AgentStatus::Running => ICON_REFRESH_CW,
        AgentStatus::Done => ICON_CIRCLE_CHECK,
        AgentStatus::Failed => ICON_CIRCLE_ALERT,
        AgentStatus::Aborted => ICON_CIRCLE_STOP,
        AgentStatus::Interrupted => ICON_TRIANGLE_ALERT,
    }
}

/// What one agent shows as its state: omp's live word where it knows better than the files
/// (`parked`, `idle`), else the file-inferred status.
pub struct Shown {
    pub label: &'static str,
    pub icon: Icon,
    pub color: Color32,
    pub spin: bool,
}

pub fn shown(pal: &Palette, status: AgentStatus, live: Option<LiveStatus>) -> Shown {
    match live {
        Some(LiveStatus::Parked) => Shown {
            label: "parked",
            icon: ICON_CIRCLE_PAUSE,
            color: theme::mix(pal.weak, pal.accent, 0.4),
            spin: false,
        },
        Some(LiveStatus::Idle) if status == AgentStatus::Done => Shown {
            label: "idle",
            icon: ICON_MOON_STAR,
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

/// Per-session state of the rows: collapsed groups and the cached geometry.
#[derive(Default)]
pub struct RowsState {
    collapsed: HashSet<String>,
    collapsed_gen: u64,
    order: Vec<usize>,
    order_key: Option<(u64, bool, u64)>,
    offsets: Vec<f32>,
    /// Measured gist galleys by snapshot index; valid for `gist_key`.
    gists: Vec<Option<Arc<Galley>>>,
    gist_key: Option<(u64, i32, bool, bool)>,
    /// Height of all rows as last prepared.
    total: f32,
}

impl RowsState {
    /// Orders the rows for `view` and measures them for a list `width` wide; returns the total
    /// height. Cached per (snapshot, width, mode, theme), so calling it every frame is cheap.
    pub fn prepare(&mut self, ui: &Ui, view: &AgentsView, width: f32, tree: bool) -> f32 {
        let pal = Palette::of_ui(ui);
        let agents = view.agents();
        let key = (view.version, tree, self.collapsed_gen);
        let gist_key = (
            view.base_version,
            width.round() as i32,
            tree,
            ui.visuals().dark_mode,
        );
        if self.order_key == Some(key) && self.gist_key == Some(gist_key) {
            return self.total;
        }
        if self.order_key != Some(key) {
            self.order = agent_model::visible(agents, tree, &self.collapsed);
            self.order_key = Some(key);
        }
        if self.gist_key != Some(gist_key) {
            self.gists.clear();
            self.gists.resize(agents.len(), None);
            self.gist_key = Some(gist_key);
        }
        self.offsets.clear();
        self.offsets.push(0.0);
        let mut y = 0.0;
        for k in 0..self.order.len() {
            let i = self.order[k];
            let gist = &view.cards[i].gist;
            if !gist.is_empty() && self.gists[i].is_none() {
                let depth = if tree { agents[i].depth.min(6) } else { 0 };
                self.gists[i] = Some(layout_gist(ui, pal, gist, gist_wrap_width(width, depth)));
            }
            y += card_height(&agents[i], self.gists[i].as_ref()) + card_gap();
            self.offsets.push(y);
        }
        self.total = y;
        y
    }
}

pub enum RowAction {
    Open(String),
    /// Ask to kill this agent (the app confirms first).
    Kill(String),
    Revive(String),
}

pub struct RowsInput<'a> {
    pub view: &'a AgentsView,
    pub tree: bool,
    /// Agent whose popup is open (highlighted).
    pub selected: Option<&'a str>,
    pub ctl: &'a CtlView<'a>,
}

/// Paints the rows prepared by [`RowsState::prepare`] for the same `width` and mode, `width` wide
/// at the cursor of `ui`; only the rows near the clip rect are drawn.
pub fn rows(
    ui: &mut Ui,
    st: &mut RowsState,
    input: &RowsInput<'_>,
    width: f32,
    out: &mut Vec<RowAction>,
) {
    let view = input.view;
    let agents = view.agents();
    let n = st.order.len();
    if st.offsets.len() != n + 1 {
        return;
    }
    let tree = input.tree;
    let (area, _) = ui.allocate_exact_size(vec2(width, st.total), Sense::hover());
    let clip = ui.clip_rect();
    let lo = clip.min.y - area.min.y - OVERSCAN;
    let hi = clip.max.y - area.min.y + OVERSCAN;
    let first = st.offsets[1..].partition_point(|&o| o <= lo).min(n);
    let end = st.offsets[..n].partition_point(|&o| o < hi).max(first);
    let mut toggled = None;
    for r in first..end {
        let i = st.order[r];
        let (Some(a), Some(c)) = (agents.get(i), view.cards.get(i)) else {
            continue;
        };
        let row = Rect::from_min_size(
            pos2(area.min.x, area.min.y + st.offsets[r]),
            vec2(width, st.offsets[r + 1] - st.offsets[r]),
        );
        let depth = if tree { a.depth.min(6) } else { 0 };
        let opts = CardOpts {
            tree_depth: tree.then_some(depth),
            selected: input.selected == Some(a.id.as_str()),
            act: card_act(input.ctl, a, c),
        };
        match card(
            ui,
            row,
            a,
            c,
            st.gists.get(i).and_then(Option::as_ref),
            opts,
        ) {
            CardClick::Open => out.push(RowAction::Open(a.id.clone())),
            CardClick::ToggleChildren => toggled = Some(a.id.clone()),
            CardClick::Act(CardAct::Kill) => out.push(RowAction::Kill(a.id.clone())),
            CardClick::Act(CardAct::Revive) => out.push(RowAction::Revive(a.id.clone())),
            CardClick::None => {}
        }
    }
    if let Some(id) = toggled {
        if !st.collapsed.remove(&id) {
            st.collapsed.insert(id);
        }
        st.collapsed_gen += 1;
    }
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
        crate::theme::font(12.5),
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
        pos2(row.right() - 2.0, row.bottom() - card_gap()),
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
        AgentStatus::Running => theme::accent_wash(pal.surface, pal.accent_b, 0.06),
        AgentStatus::Failed => theme::mix(pal.surface, pal.danger, 0.07),
        AgentStatus::Aborted | AgentStatus::Interrupted => theme::mix(pal.surface, pal.warn, 0.05),
        AgentStatus::Done => pal.surface,
    };
    let fill = if hovered {
        theme::mix(tint, pal.raised, 0.7)
    } else {
        tint
    };
    theme::acc::paint_sub(&p, id, pal, rect, hovered, selected, fill);
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
    let id_g = p.layout_no_wrap(a.id.clone(), crate::theme::font(13.5), pal.text_strong);
    let id_w = id_g.size().x;
    // Model chip on the right (dropped when it would crowd the id).
    let model_badge = (!c.short_model.is_empty()).then(|| {
        Badge::single(
            &p,
            Some(ICON_CPU),
            &c.short_model,
            Hue::Violet.color(pal),
            11.0,
        )
    });
    let mut limit = right;
    if let Some(b) = &model_badge
        && x + id_w + 24.0 + b.width() < right
    {
        let r = b.paint_at(&p, right - b.width(), mid);
        limit = r.left() - 6.0;
    }
    let id_galley = if x + id_w > limit {
        // Ellipsize a very long id.
        let mut job =
            LayoutJob::simple_singleline(a.id.clone(), crate::theme::font(13.5), pal.text_strong);
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
        (
            a.agent.as_deref(),
            if theme::flat() { pal.text } else { pal.accent },
        ),
        (a.advisor.then_some("advisor"), pal.warn),
    ];
    for (text, col) in badges {
        let Some(text) = text else { continue };
        let g = p.layout_no_wrap(text.to_owned(), crate::theme::font(10.5), col);
        let w = g.size().x + 12.0;
        if x + w > limit {
            break;
        }
        let r = Rect::from_min_size(pos2(x, mid - 8.0), vec2(w, 16.0));
        let badge_fill = if theme::flat() && col == pal.text {
            pal.raised
        } else {
            col.gamma_multiply(0.16)
        };
        p.rect_filled(r, 8, badge_fill);
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
                font_id: crate::theme::font(11.5),
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
    let age_g = p.layout_no_wrap(c.age.clone(), crate::theme::font(11.5), pal.weak);
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
            CardAct::Kill => (ICON_SQUARE, "Kill this agent…", pal.danger),
            CardAct::Revive => (ICON_PLAY, "Revive this agent", pal.ok),
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
        let g = p.layout_no_wrap(label, crate::theme::font(11.5), pal.text);
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
            ICON_CHEVRON_DOWN,
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
    // One badge per metric, each in its own hue; the ones that do not fit are dropped.
    let mut bx = left;
    for m in &c.metrics {
        let hue = match m.kind {
            MetricKind::Cost => Hue::Green,
            MetricKind::Time => Hue::Amber,
            MetricKind::Requests => Hue::Blue,
            MetricKind::Tools => Hue::Orange,
            MetricKind::Tokens => Hue::Teal,
        };
        let b = Badge::single(&p, None, &m.text, hue.color(pal), 11.0);
        if bx + b.width() > rx - 4.0 {
            break;
        }
        b.paint_at(&p, bx, my);
        bx += b.width() + 5.0;
    }

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
