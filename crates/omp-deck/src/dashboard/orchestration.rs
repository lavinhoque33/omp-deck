//! The Orchestration tab of the agents dashboard: every session that opened sessions of its own
//! (`ompdeck_open_session` / `ompdeck_fan_out`) as a card, its children hanging off a Branch
//! Ledger rail. The gateway publishes the tree (`Topic::Orchestration`, followed only while this
//! tab is on screen); [`OrchState::set_view`] flattens it once per revision, so painting
//! allocates nothing per frame beyond the galleys.
//!
//! The pure parts (flattening, totals, counts) are at the top and unit-tested.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use deck_core::ctl::Progress;
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{
    OrchFinish, OrchNode, OrchReport, OrchWorktree, OrchestrationView,
};
use deck_service::status::TabStatus;
use deck_service::statusfmt::{fmt_cost, short_model_name};
use egui::{
    Align, Align2, CursorIcon, Frame, Id, Layout, Margin, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, Ui, pos2, vec2,
};

use super::DashAction;
use crate::badge::{Badge, Hue};
use crate::icons::{
    ICON_BRAIN, ICON_CIRCLE_STOP, ICON_COLUMNS_2, ICON_CPU, ICON_EXTERNAL_LINK, ICON_FOLDER,
    ICON_GIT_BRANCH, ICON_GIT_MERGE, ICON_HOURGLASS, ICON_NETWORK, ICON_WORKFLOW, Icon,
};
use crate::model_picker::ModelsCache;
use crate::progress_view;
use crate::sidebar::{format_age, paint_elbow, rail_color};
use crate::tabs::paint_status;
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, DraftKind, Spin, empty_state, icon_text, menu_item, paint_icon};

/// Height of a child row without / with its last-report line.
const ROW_H: f32 = 34.0;
const REPORT_H: f32 = 20.0;
/// Horizontal step per nesting level.
const INDENT: f32 = 22.0;
/// Rail position inside a level's indent.
const RAIL_DX: f32 = 9.0;
/// Width of the row hover buttons.
const BTN: f32 = 24.0;

// ------------------------------------------------------------------ pure parts

/// Where a node stands, for the card's counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Attention,
    Busy,
    Idle,
    Ended,
}

impl Bucket {
    pub fn of(n: &OrchNode) -> Bucket {
        match n.status {
            _ if !n.live => Bucket::Ended,
            TabStatus::Exited { .. } => Bucket::Ended,
            TabStatus::Attention | TabStatus::Bell => Bucket::Attention,
            TabStatus::Busy => Bucket::Busy,
            TabStatus::Idle if n.busy => Bucket::Busy,
            TabStatus::Idle => Bucket::Idle,
        }
    }

    const EACH: [Bucket; 4] = [Bucket::Attention, Bucket::Busy, Bucket::Idle, Bucket::Ended];

    fn label(self) -> &'static str {
        match self {
            Bucket::Attention => "need you",
            Bucket::Busy => "working",
            Bucket::Idle => "idle",
            Bucket::Ended => "ended",
        }
    }
}

/// One node without its children, the model and thinking already short.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub handle: Option<String>,
    pub target: Option<SessionTarget>,
    pub session_path: PathBuf,
    pub title: String,
    pub cwd: PathBuf,
    pub status: TabStatus,
    pub live: bool,
    pub bucket: Bucket,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub progress: Option<Progress>,
    /// `progress_view::counts_text` and `ratio` of `progress`.
    pub progress_text: Option<String>,
    pub progress_ratio: Option<f32>,
    pub report: Option<OrchReport>,
    pub worktree: Option<OrchWorktree>,
    pub finish: Option<OrchFinish>,
    /// [`finish_chip`] of `finish`.
    pub finish_chip: Option<(String, Hue)>,
    /// "fan-out <id>" of the fan-out the node belongs to.
    pub fan_out: Option<String>,
    pub cost: Option<f64>,
}

impl Node {
    fn of(n: &OrchNode, short: &impl Fn(&str) -> String) -> Node {
        Node {
            handle: n.handle.clone(),
            target: n.target.clone(),
            session_path: n.session_path.clone(),
            title: if n.title.trim().is_empty() {
                n.session_path
                    .file_stem()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
            } else {
                n.title.clone()
            },
            cwd: n.cwd.clone(),
            status: n.status,
            live: n.live,
            bucket: Bucket::of(n),
            model: n.model.as_deref().map(short),
            thinking: n.thinking.clone().filter(|t| t != "off"),
            progress_text: n.progress.as_ref().and_then(progress_view::counts_text),
            progress_ratio: n.progress.as_ref().and_then(progress_view::ratio),
            progress: n.progress.clone(),
            report: n.last_report.clone(),
            worktree: n.worktree.clone(),
            finish: n.finish.clone(),
            finish_chip: n.finish.as_ref().map(finish_chip),
            fan_out: n.fan_out.as_ref().map(|f| format!("fan-out {f}")),
            cost: n.cost,
        }
    }

    /// "Finish worktree" applies: a live worktree child whose finish is not running.
    pub fn can_finish(&self) -> bool {
        self.live
            && self.worktree.is_some()
            && !matches!(
                self.finish,
                Some(OrchFinish::Queued { .. } | OrchFinish::Merging)
            )
    }

    fn reference(&self) -> NodeRef {
        NodeRef {
            target: self.target.clone(),
            session: self.session_path.clone(),
            title: self.title.clone(),
        }
    }
}

/// A child row: the node, how deep it hangs, whether it is its parent's last child, and for
/// each ancestor level above its own whether that level's rail continues below it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub depth: usize,
    pub last: bool,
    pub guides: Vec<bool>,
    pub node: Node,
}

/// One root: the session that opened children, its whole tree's cost and its children's counts.
#[derive(Clone, Debug, PartialEq)]
pub struct RootCard {
    pub root: Node,
    pub total_cost: Option<f64>,
    /// Children per [`Bucket::EACH`].
    pub counts: [usize; 4],
    pub rows: Vec<Row>,
}

/// Flattens the gateway's tree into cards (roots in the gateway's order, children depth first).
pub fn build(view: &OrchestrationView, short: &impl Fn(&str) -> String) -> Vec<RootCard> {
    view.roots
        .iter()
        .map(|r| {
            let mut rows = Vec::new();
            let mut guides = Vec::new();
            flatten(&r.children, 1, &mut guides, &mut rows, short);
            let mut counts = [0; 4];
            for row in &rows {
                let i = Bucket::EACH
                    .iter()
                    .position(|b| *b == row.node.bucket)
                    .unwrap_or(0);
                counts[i] += 1;
            }
            RootCard {
                root: Node::of(r, short),
                total_cost: tree_cost(r),
                counts,
                rows,
            }
        })
        .collect()
}

fn flatten(
    children: &[OrchNode],
    depth: usize,
    guides: &mut Vec<bool>,
    rows: &mut Vec<Row>,
    short: &impl Fn(&str) -> String,
) {
    for (i, c) in children.iter().enumerate() {
        let last = i + 1 == children.len();
        rows.push(Row {
            depth,
            last,
            guides: guides.clone(),
            node: Node::of(c, short),
        });
        if !c.children.is_empty() {
            guides.push(!last);
            flatten(&c.children, depth + 1, guides, rows, short);
            guides.pop();
        }
    }
}

/// The cost of a node and everything below it; `None` when no node reports one.
pub fn tree_cost(n: &OrchNode) -> Option<f64> {
    let below = n.children.iter().filter_map(tree_cost);
    let mut any = n.cost.is_some();
    let mut sum = n.cost.unwrap_or(0.0);
    for c in below {
        any = true;
        sum += c;
    }
    any.then_some(sum)
}

/// The finish chip's text and tone.
pub fn finish_chip(f: &OrchFinish) -> (String, Hue) {
    match f {
        OrchFinish::Queued { ahead } => (format!("Queued ({ahead} ahead)"), Hue::Amber),
        OrchFinish::Merging => ("Merging".to_owned(), Hue::Blue),
        OrchFinish::Blocked { .. } => ("Blocked".to_owned(), Hue::Rose),
        OrchFinish::Done { .. } => ("Done".to_owned(), Hue::Green),
    }
}

/// The report kind's badge tone.
fn report_hue(kind: &str) -> Hue {
    match kind {
        "done" => Hue::Green,
        "blocked" => Hue::Rose,
        "progress" => Hue::Blue,
        _ => Hue::Violet,
    }
}

// ------------------------------------------------------------------ state

/// What the tab remembers: the flattened tree of the last revision read.
#[derive(Default)]
pub struct OrchState {
    /// This window follows `Topic::Orchestration`.
    pub subscribed: bool,
    /// Topic revision the cards were built from (0: nothing yet).
    pub rev: u64,
    pub cards: Vec<RootCard>,
}

impl OrchState {
    pub fn set_view(&mut self, rev: u64, view: &OrchestrationView, models: &ModelsCache) {
        let short = |sel: &str| {
            models.get(sel).map_or_else(
                || crate::agent_model::short_model(sel),
                |m| short_model_name(&m.name).to_owned(),
            )
        };
        self.cards = build(view, &short);
        self.rev = rev;
    }

    /// The tab left the screen: drop what was read (the topic is unsubscribed by the app).
    pub fn clear(&mut self) {
        self.subscribed = false;
        self.rev = 0;
        self.cards.clear();
    }
}

/// A session the user acts on.
#[derive(Clone, Debug)]
pub struct NodeRef {
    pub target: Option<SessionTarget>,
    pub session: PathBuf,
    pub title: String,
}

/// What a card or row asked for.
#[derive(Debug)]
pub enum OrchAction {
    /// Focus its tab, or open it.
    Open(NodeRef),
    /// Open it in a pane to the right.
    OpenSplit(NodeRef),
    /// Finish (commit + merge) its worktree: the sidebar's gated path.
    Finish(PathBuf),
    Stop(NodeRef),
    /// Stop every live child of a card (asks first, listing them).
    StopAll(Vec<NodeRef>),
}

// ------------------------------------------------------------------ view

/// Draws the tab into the current ui.
pub fn show(ui: &mut Ui, st: &OrchState, out: &mut Vec<DashAction>) {
    let pal = Palette::of_ui(ui);
    if st.rev == 0 {
        ui.horizontal(|ui| {
            ui.add(Spin::new(16.0, pal.accent));
            ui.label(RichText::new("Reading the session tree…").color(pal.weak));
        });
        return;
    }
    if st.cards.is_empty() {
        empty_state(
            ui,
            ICON_WORKFLOW,
            pal.accent,
            "No orchestrated sessions",
            "When an agent opens sessions of its own — ompdeck_open_session for one, ompdeck_fan_out for up to 8 in parallel worktrees — they show here as a tree under the session that opened them, with their progress, reports, finish state and cost.",
        );
        return;
    }
    let now = SystemTime::now();
    ScrollArea::vertical()
        .id_salt("orchestration")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 12.0;
            for (i, card) in st.cards.iter().enumerate() {
                root_card(ui, i, card, now, out);
            }
        });
    // Report ages move with the clock.
    ui.ctx().request_repaint_after(Duration::from_secs(30));
}

fn root_card(
    ui: &mut Ui,
    index: usize,
    card: &RootCard,
    now: SystemTime,
    out: &mut Vec<DashAction>,
) {
    let pal = Palette::of_ui(ui);
    let id = Id::unique(("orch-card", index));
    Frame::NONE
        .fill(pal.surface)
        .stroke(Stroke::new(1.0, theme::mix(pal.stroke, pal.weak, 0.35)))
        .corner_radius(theme::ui_rad(10))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let header = card_header(ui, card, out);
            // The rail drops from the root's status marker.
            let rail_top = header.left_center().y + 8.0;
            ui.add_space(8.0);
            let rows_top = ui.cursor().top();
            let left = ui.max_rect().left();
            for row in &card.rows {
                child_row(ui, id, row, left, now, out);
            }
            if card.rows.is_empty() {
                return;
            }
            // The root's rail drops from its status marker to the first child's row, where the
            // rows carry it on.
            let rail_x = left + RAIL_DX;
            if rows_top > rail_top {
                ui.painter().vline(
                    rail_x,
                    rail_top..=rows_top,
                    Stroke::new(1.0, rail_color(pal)),
                );
            }
        });
}

/// Title + model/thinking, project, counts, total cost and "Stop all children…".
fn card_header(ui: &mut Ui, card: &RootCard, out: &mut Vec<DashAction>) -> Rect {
    let pal = Palette::of_ui(ui);
    let root = &card.root;
    let resp = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (marker, _) = ui.allocate_exact_size(vec2(18.0, 22.0), Sense::hover());
        paint_status(ui, marker.center(), 15.0, root.status);
        let title = ui
            .add(
                egui::Label::new(
                    theme::strong(root.title.as_str())
                        .size(14.5)
                        .color(pal.text_strong),
                )
                .truncate()
                .sense(Sense::click()),
            )
            .on_hover_text("Open this session")
            .on_hover_cursor(CursorIcon::PointingHand);
        if title.clicked() {
            out.push(DashAction::Orch(OrchAction::Open(root.reference())));
        }
        model_badges(ui, root);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let live = |r: &&Row| r.node.live;
            if DraftButton::new("Stop all children…")
                .icon(ICON_CIRCLE_STOP)
                .kind(DraftKind::Danger)
                .enabled(card.rows.iter().any(|r| r.node.live))
                .show(ui)
                .on_hover_text("Stop every running session this one opened (asks first)")
                .on_disabled_hover_text("None of its sessions is running")
                .clicked()
            {
                // Children without a live target resolve their backend through the session file.
                let nodes = card.rows.iter().filter(live).map(|r| r.node.reference());
                out.push(DashAction::Orch(OrchAction::StopAll(nodes.collect())));
            }
            if let Some(c) = card.total_cost {
                crate::badge::show(ui, None, &fmt_cost(c), Hue::Green, 11.5)
                    .on_hover_text("Cost of this session and every session below it");
            }
        });
    });
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(24.0);
        ui.label(icon_text(ICON_FOLDER, 11.5, pal.weak));
        ui.label(
            RichText::new(deck_core::sessions::abbreviate_home(&root.cwd))
                .font(theme::label_font(11.0))
                .color(pal.weak),
        );
        ui.add_space(6.0);
        for (i, b) in Bucket::EACH.iter().enumerate() {
            let n = card.counts[i];
            if n == 0 {
                continue;
            }
            let hue = match b {
                Bucket::Attention => Hue::Amber,
                Bucket::Busy => Hue::Blue,
                Bucket::Idle => Hue::Teal,
                Bucket::Ended => Hue::Violet,
            };
            crate::badge::show(ui, None, &format!("{n} {}", b.label()), hue, 10.5);
        }
    });
    resp.response.rect
}

fn model_badges(ui: &mut Ui, n: &Node) {
    if let Some(m) = &n.model {
        crate::badge::show(ui, Some(ICON_CPU), m, Hue::Violet, 11.0);
    }
    if let Some(t) = &n.thinking {
        crate::badge::show(ui, Some(ICON_BRAIN), t, Hue::Rose, 11.0);
    }
}

/// One child: rail/elbow, status marker, title + model/thinking, branch, finish, progress, cost;
/// the last report below. Hover shows the actions the context menu offers too.
fn child_row(
    ui: &mut Ui,
    card: Id,
    row: &Row,
    left: f32,
    now: SystemTime,
    out: &mut Vec<DashAction>,
) {
    let pal = Palette::of_ui(ui);
    let n = &row.node;
    let h = ROW_H + if n.report.is_some() { REPORT_H } else { 0.0 };
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
    let id = card.with(&n.session_path);
    let hovered = resp.hovered() || ui.rect_contains_pointer(rect);
    if !ui.is_rect_visible(rect) {
        row_menu(&resp, n, out);
        return;
    }
    let p = ui.painter();
    let rail = rail_color(pal);
    let stub_y = rect.top() + ROW_H / 2.0;
    // Ancestor rails that continue past this row.
    for (level, cont) in row.guides.iter().enumerate() {
        if *cont {
            let x = left + level as f32 * INDENT + RAIL_DX;
            p.vline(x, rect.top()..=rect.bottom(), Stroke::new(1.0, rail));
        }
    }
    let rail_x = left + (row.depth - 1) as f32 * INDENT + RAIL_DX;
    let box_x = left + row.depth as f32 * INDENT;
    if row.last {
        paint_elbow(p, rail_x, rect.top(), box_x - 2.0, stub_y, rail);
    } else {
        p.vline(rail_x, rect.top()..=rect.bottom(), Stroke::new(1.0, rail));
        p.hline(rail_x..=box_x - 2.0, stub_y, Stroke::new(1.0, rail));
    }
    // The row box: the outline pops on hover.
    let body = Rect::from_min_max(
        pos2(box_x, rect.top() + 1.0),
        pos2(rect.right(), rect.bottom() - 1.0),
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(id.with("hov"), hovered, 0.1);
    let r = theme::ui_rad(7);
    p.rect_filled(
        body,
        r,
        theme::hover_fill(pal).gamma_multiply(0.25 + 0.6 * hov),
    );
    p.rect_stroke(
        body,
        r,
        Stroke::new(
            1.0,
            theme::mix(theme::mix(pal.stroke, pal.weak, 0.25), pal.accent, hov),
        ),
        StrokeKind::Inside,
    );
    paint_status(ui, pos2(body.left() + 13.0, stub_y), 13.0, n.status);
    let p = ui.painter();
    // Right side first: cost, progress (they keep their room; the title is cut).
    let mut right = body.right() - 8.0;
    let cy = stub_y;
    if hovered {
        right = hover_buttons(ui, body, cy, n, out);
    } else {
        if let Some(c) = n.cost {
            let b = Badge::single(p, None, &fmt_cost(c), Hue::Green.color(pal), 10.5);
            right -= b.width();
            b.paint_at(p, right, cy);
            right -= 8.0;
        }
        if let Some(text) = &n.progress_text {
            let g = p.layout_no_wrap(text.clone(), theme::label_font(10.5), pal.weak);
            right -= g.size().x;
            p.galley(pos2(right, cy - g.size().y / 2.0), g, pal.weak);
            right -= 5.0;
            let bar = Rect::from_min_size(pos2(right - 46.0, cy - 2.5), vec2(46.0, 5.0));
            progress_view::paint_mini_bar(p, pal, bar, n.progress_ratio.unwrap_or(0.0));
            right = bar.left() - 8.0;
        }
    }
    let p = ui.painter();
    let mut x = body.left() + 26.0;
    let clip = Rect::from_min_max(pos2(x, body.top()), pos2(right, body.bottom()));
    let pc = p.with_clip_rect(clip);
    let title = pc.layout_no_wrap(n.title.clone(), theme::font(12.5), pal.text_strong);
    let tw = title.size().x;
    pc.galley(pos2(x, cy - title.size().y / 2.0), title, pal.text_strong);
    x += tw + 8.0;
    let mut badge = |icon: Icon, text: &str, hue: Hue| {
        let b = Badge::single(&pc, Some(icon), text, hue.color(pal), 10.5);
        b.paint_at(&pc, x, cy);
        x += b.width() + 5.0;
    };
    if let Some(m) = &n.model {
        badge(ICON_CPU, m, Hue::Violet);
    }
    if let Some(t) = &n.thinking {
        badge(ICON_BRAIN, t, Hue::Rose);
    }
    if let Some(b) = n.worktree.as_ref().and_then(|w| w.branch.as_deref()) {
        badge(ICON_GIT_BRANCH, b, Hue::Teal);
    }
    if let (Some(f), Some((text, hue))) = (&n.finish, &n.finish_chip) {
        let icon = if matches!(f, OrchFinish::Queued { .. }) {
            ICON_HOURGLASS
        } else {
            ICON_GIT_MERGE
        };
        badge(icon, text, *hue);
    }
    if let Some(f) = &n.fan_out {
        badge(ICON_NETWORK, f, Hue::Blue);
    }
    // The last report: kind badge, text, age.
    if let Some(rep) = &n.report {
        let ry = rect.top() + ROW_H + REPORT_H / 2.0 - 4.0;
        let x0 = body.left() + 26.0;
        let pr = p.with_clip_rect(Rect::from_min_max(
            pos2(x0, body.top()),
            pos2(body.right() - 8.0, body.bottom()),
        ));
        let kind = Badge::single(&pr, None, &rep.kind, report_hue(&rep.kind).color(pal), 10.0);
        kind.paint_at(&pr, x0, ry);
        let age = format_age(
            now,
            SystemTime::UNIX_EPOCH + Duration::from_millis(rep.at_unix_ms),
        );
        let age = pr.layout_no_wrap(age, theme::label_font(10.5), pal.weak);
        let age_w = age.size().x;
        pr.galley(
            pos2(body.right() - 8.0 - age_w, ry - age.size().y / 2.0),
            age,
            pal.weak,
        );
        let text_x = x0 + kind.width() + 6.0;
        let line = rep.text.lines().next().unwrap_or_default();
        let pt = pr.with_clip_rect(Rect::from_min_max(
            pos2(text_x, body.top()),
            pos2(body.right() - 14.0 - age_w, body.bottom()),
        ));
        let g = pt.layout_no_wrap(line.to_owned(), theme::font(11.5), pal.text);
        pt.galley(pos2(text_x, ry - g.size().y / 2.0), g, pal.text);
    }
    // Built only while the tooltip shows.
    let resp = if let Some(rep) = &n.report {
        resp.on_hover_ui(|ui| {
            ui.label(format!("{}: {}", rep.kind, rep.text));
        })
    } else {
        resp
    };
    if resp.double_clicked() {
        out.push(DashAction::Orch(OrchAction::Open(n.reference())));
    }
    row_menu(&resp, n, out);
}

/// The row's hover buttons (right-aligned, vertically centred on the title line); returns the
/// x left of them.
fn hover_buttons(ui: &mut Ui, body: Rect, cy: f32, n: &Node, out: &mut Vec<DashAction>) -> f32 {
    let mut right = body.right() - 6.0;
    let mut place = |ui: &mut Ui, icon: Icon, tip: &str, enabled: bool, danger: bool| -> bool {
        let r = Rect::from_center_size(pos2(right - BTN / 2.0, cy), vec2(BTN, BTN));
        right -= BTN + 3.0;
        row_button(ui, r, icon, tip, enabled, danger)
    };
    if place(ui, ICON_CIRCLE_STOP, stop_tip(n), n.live, true) {
        out.push(DashAction::Orch(OrchAction::Stop(n.reference())));
    }
    if n.worktree.is_some()
        && place(ui, ICON_GIT_MERGE, finish_tip(n), n.can_finish(), false)
        && let Some(w) = &n.worktree
    {
        out.push(DashAction::Orch(OrchAction::Finish(w.path.clone())));
    }
    if place(ui, ICON_COLUMNS_2, "Open in split right", true, false) {
        out.push(DashAction::Orch(OrchAction::OpenSplit(n.reference())));
    }
    if place(
        ui,
        ICON_EXTERNAL_LINK,
        "Open (focus its tab or open it)",
        true,
        false,
    ) {
        out.push(DashAction::Orch(OrchAction::Open(n.reference())));
    }
    right - 4.0
}

fn stop_tip(n: &Node) -> &'static str {
    if n.live {
        "Stop session (asks first)"
    } else {
        "The session is not running"
    }
}

fn finish_tip(n: &Node) -> &'static str {
    match &n.finish {
        Some(OrchFinish::Queued { .. }) => "Already queued to finish",
        Some(OrchFinish::Merging) => "Already finishing",
        _ if !n.live => "The session is not running",
        _ => "Finish worktree: ask its agent to commit and merge, then remove the worktree",
    }
}

fn row_button(ui: &mut Ui, rect: Rect, icon: Icon, tip: &str, enabled: bool, danger: bool) -> bool {
    let pal = Palette::of_ui(ui);
    let resp = ui.interact(
        rect,
        ui.scope_id()
            .with(("orch-btn", rect.min.x as i32, rect.min.y as i32)),
        Sense::click(),
    );
    let hov = resp.hovered() && enabled;
    let tint = if !enabled {
        pal.weak.gamma_multiply(0.5)
    } else if danger {
        pal.danger
    } else {
        pal.glyph
    };
    let p = ui.painter();
    p.rect(
        rect,
        theme::ui_rad(5),
        if hov {
            theme::hover_fill(pal).gamma_multiply(1.6)
        } else {
            pal.surface
        },
        Stroke::new(
            1.0,
            if hov {
                if danger { pal.danger } else { pal.accent }
            } else {
                pal.stroke
            },
        ),
        StrokeKind::Inside,
    );
    paint_icon(p, rect.center(), Align2::CENTER_CENTER, icon, 14.0, tint);
    let resp = resp.on_hover_text(tip);
    enabled && resp.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

/// The same actions as the hover buttons.
fn row_menu(resp: &egui::Response, n: &Node, out: &mut Vec<DashAction>) {
    theme::context_menu(resp, 230.0, |ui| {
        if menu_item(ui, ICON_EXTERNAL_LINK, "Open", None, true, false).clicked() {
            out.push(DashAction::Orch(OrchAction::Open(n.reference())));
            ui.close();
        }
        if menu_item(ui, ICON_COLUMNS_2, "Open in split right", None, true, false).clicked() {
            out.push(DashAction::Orch(OrchAction::OpenSplit(n.reference())));
            ui.close();
        }
        if let Some(w) = &n.worktree {
            let r = menu_item(
                ui,
                ICON_GIT_MERGE,
                "Finish worktree",
                None,
                n.can_finish(),
                false,
            )
            .on_disabled_hover_text(finish_tip(n));
            if r.clicked() {
                out.push(DashAction::Orch(OrchAction::Finish(w.path.clone())));
                ui.close();
            }
        }
        ui.separator();
        let r = menu_item(ui, ICON_CIRCLE_STOP, "Stop session…", None, n.live, true)
            .on_disabled_hover_text(stop_tip(n));
        if r.clicked() {
            out.push(DashAction::Orch(OrchAction::Stop(n.reference())));
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(title: &str, cost: Option<f64>, children: Vec<OrchNode>) -> OrchNode {
        OrchNode {
            handle: None,
            target: None,
            session_path: PathBuf::from(format!("/s/{title}.jsonl")),
            title: title.to_owned(),
            cwd: PathBuf::from("/p"),
            status: TabStatus::Idle,
            busy: false,
            live: true,
            model: Some("anthropic/claude-x".into()),
            thinking: Some("off".into()),
            progress: None,
            last_report: None,
            worktree: None,
            finish: None,
            fan_out: None,
            cost,
            opened_at_unix_ms: 0,
            children,
        }
    }

    fn short(s: &str) -> String {
        s.rsplit('/').next().unwrap_or(s).to_owned()
    }

    #[test]
    fn the_tree_flattens_depth_first_with_continuing_rails_and_last_marks() {
        // root ─ a ─ a1
        //      │    └ a2
        //      └ b
        let a = node(
            "a",
            None,
            vec![node("a1", None, vec![]), node("a2", None, vec![])],
        );
        let root = node("root", None, vec![a, node("b", None, vec![])]);
        let cards = build(&OrchestrationView { roots: vec![root] }, &short);
        let rows: Vec<(&str, usize, bool, Vec<bool>)> = cards[0]
            .rows
            .iter()
            .map(|r| (r.node.title.as_str(), r.depth, r.last, r.guides.clone()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("a", 1, false, vec![]),
                ("a1", 2, false, vec![true]),
                ("a2", 2, true, vec![true]),
                ("b", 1, true, vec![]),
            ]
        );
        // "off" thinking is not shown; the model is shortened.
        assert_eq!(cards[0].root.thinking, None);
        assert_eq!(cards[0].root.model.as_deref(), Some("claude-x"));
    }

    #[test]
    fn totals_add_the_whole_tree_and_counts_cover_the_children_only() {
        let mut busy = node("busy", Some(0.5), vec![node("deep", Some(0.25), vec![])]);
        busy.busy = true;
        let mut gone = node("gone", None, vec![]);
        gone.live = false;
        let mut asks = node("asks", Some(1.0), vec![]);
        asks.status = TabStatus::Attention;
        let root = node("root", Some(2.0), vec![busy, gone, asks]);
        let cards = build(&OrchestrationView { roots: vec![root] }, &short);
        assert_eq!(cards[0].total_cost, Some(3.75));
        // need you, working, idle (deep), ended.
        assert_eq!(cards[0].counts, [1, 1, 1, 1]);
        assert_eq!(tree_cost(&node("x", None, vec![])), None);
    }

    #[test]
    fn a_running_or_queued_finish_cannot_be_started_again() {
        let mut n = node("w", None, vec![]);
        n.worktree = Some(OrchWorktree {
            path: "/wt".into(),
            branch: Some("b".into()),
            base: None,
        });
        let mut nn = Node::of(&n, &short);
        assert!(nn.can_finish());
        nn.finish = Some(OrchFinish::Queued { ahead: 2 });
        assert!(!nn.can_finish());
        nn.finish = Some(OrchFinish::Blocked { reason: "x".into() });
        assert!(nn.can_finish(), "a blocked finish may be retried");
        assert_eq!(
            finish_chip(&OrchFinish::Queued { ahead: 2 }).0,
            "Queued (2 ahead)"
        );
    }
}
