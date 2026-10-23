//! Tab model, pure tab helpers, and the tab-strip widget (drawn inside the title bar).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use deck_core::mux::{Mux, is_noise_title};
use deck_core::omp::UpdateKind;
use deck_term::Terminal;

use crate::icons::{
    ICON_CIRCLE_CHECK, ICON_CIRCLE_X, ICON_DOWNLOAD, ICON_SETTINGS, ICON_TERMINAL, ICON_X, Icon,
};
use crate::inject::Injector;
use crate::model_picker::Pending;
use crate::statusfmt::Activity;
use crate::titlebar::strip_omp_prefix;
use egui::{
    Align2, Color32, CursorIcon, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::sidebar::{self, MenuEnv, SidebarAction, SidebarOutput};
use crate::theme::{self, Palette};
use crate::widgets::paint_icon;

/// What a terminal tab runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabRole {
    /// An omp session (resumed, new or forked).
    Session,
    /// `omp update …` (never persisted).
    Update(UpdateKind),
    /// omp's official installer, offered while omp is not found (never persisted).
    InstallOmp,
}

/// A terminal tab: an omp process (usually in a tmux or host session) rendered by deck-term.
pub struct TermTab {
    pub term: Terminal,
    /// Session name in the backend; `None` for direct tabs (update/installer, or no backend).
    pub mux_name: Option<String>,
    /// The backend running the session (`None` with `mux_name`).
    pub mux: Option<Mux>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    /// Label used until the sidebar knows this session (restored state / click time).
    pub fallback_title: String,
    pub bell: bool,
    pub role: TabRole,
    /// The exit of an update tab has been acted upon.
    pub exit_handled: bool,
    /// Set while this tab runs `omp --fork` and the copy's session file is not known yet.
    pub copy: Option<CopyPending>,
    /// Stable identity of the tab (indices shift when tabs close).
    pub uid: u64,
    /// Queued composer/hotkey injections (status-bar actions).
    pub injector: Injector,
    /// A model/thinking change sent to omp and not yet seen in the session file.
    pub pending_switch: Option<Pending>,
    /// The last confirmed-by-timeout switch, shown while the session file records no model yet.
    pub assumed_switch: Option<Pending>,
    /// Key of this tab's control socket (`deck_core::ctl::ctl_dir(key)`) when it was launched
    /// with the control extension; `None` for update tabs and when no extension is installed.
    pub ctl_key: Option<String>,
    /// This launch created the session with the extension, so its socket is expected soon
    /// (attaching to an existing session ignores the argv: no expectation).
    pub ctl_expect: bool,
    /// The control socket was connected at least once.
    pub ctl_seen: bool,
    /// When the tab was spawned (a freshly launched omp needs a moment to open its socket).
    pub spawned: Instant,
}

/// A copy in progress: the tab runs `omp --fork <src>`; the new session file is found by
/// polling the sessions root (see `deck_core::sessions::find_fork`).
pub struct CopyPending {
    /// Identifies the copy in worker results.
    pub id: u64,
    /// Session id of the source (the fork's `parentSession`).
    pub src_id: String,
    /// Wall-clock start, the lower bound for the new file's mtime.
    pub started_at: SystemTime,
    pub started: Instant,
    pub next_poll: Instant,
    /// A poll is running on a worker.
    pub polling: bool,
}

pub enum Tab {
    Terminal(Box<TermTab>),
    /// The preferences page (at most one; never persisted).
    Settings,
}

impl Tab {
    pub fn term(&self) -> Option<&TermTab> {
        match self {
            Tab::Terminal(t) => Some(t),
            Tab::Settings => None,
        }
    }

    pub fn term_mut(&mut self) -> Option<&mut TermTab> {
        match self {
            Tab::Terminal(t) => Some(t),
            Tab::Settings => None,
        }
    }

    /// The terminal tab's uid.
    pub fn uid(&self) -> Option<u64> {
        self.term().map(|t| t.uid)
    }

    pub fn is_settings(&self) -> bool {
        matches!(self, Tab::Settings)
    }

    pub fn session_path(&self) -> Option<&Path> {
        self.term().and_then(|t| t.session_path.as_deref())
    }

    pub fn mux_name(&self) -> Option<&str> {
        self.term().and_then(|t| t.mux_name.as_deref())
    }

    pub fn title<'a>(&'a self, labels: &'a HashMap<PathBuf, String>) -> &'a str {
        match self {
            Tab::Terminal(t) => t.title(labels),
            Tab::Settings => "Settings",
        }
    }

    /// The title without omp's `π <glyph>` prefix: the status indicator shows that state.
    pub fn label<'a>(&'a self, labels: &'a HashMap<PathBuf, String>) -> &'a str {
        match self {
            // Restored tabs fall back to the stored (raw) title until omp sets one.
            Tab::Terminal(t) => {
                strip_omp_prefix(t.pick_title(labels, t.term.title().map(strip_omp_prefix)))
            }
            Tab::Settings => "Settings",
        }
    }

    /// The leading indicator; the bell only counts on a background tab.
    pub fn status(&self, active: bool) -> TabStatus {
        match self.term() {
            Some(t) => TabStatus::of(
                t.term.exit_code(),
                Activity::from_title(t.term.title()),
                t.bell && !active,
            ),
            None => TabStatus::Idle,
        }
    }

    /// The idle icon and its accent (`sel` = selection 0..=1, mixing from `weak`).
    pub fn kind_icon(&self, pal: &Palette, sel: f32) -> (Icon, Color32) {
        match self {
            Tab::Settings => (ICON_SETTINGS, theme::mix(pal.weak, pal.accent, sel)),
            Tab::Terminal(t) if matches!(t.role, TabRole::Update(_) | TabRole::InstallOmp) => (
                ICON_DOWNLOAD,
                theme::mix(pal.weak, pal.accent_a, sel.max(0.6)),
            ),
            Tab::Terminal(_) => (ICON_TERMINAL, theme::mix(pal.weak, pal.accent_b, sel)),
        }
    }
}

impl TermTab {
    /// OSC title if non-empty, else the session label, else the stored title, else cwd basename.
    pub fn title<'a>(&'a self, labels: &'a HashMap<PathBuf, String>) -> &'a str {
        self.pick_title(labels, self.term.title())
    }

    fn pick_title<'a>(
        &'a self,
        labels: &'a HashMap<PathBuf, String>,
        osc: Option<&'a str>,
    ) -> &'a str {
        match self.role {
            TabRole::Update(kind) => return crate::update::tab_title(kind),
            TabRole::InstallOmp => return "Install omp",
            TabRole::Session => {}
        }
        osc.filter(|t| !is_noise_title(t))
            .or_else(|| {
                self.session_path
                    .as_ref()
                    .and_then(|p| labels.get(p))
                    .map(String::as_str)
            })
            .or_else(|| Some(self.fallback_title.as_str()).filter(|t| !t.is_empty()))
            .or_else(|| self.cwd.file_name().and_then(|n| n.to_str()))
            .unwrap_or("omp")
    }
}

/// What a tab's leading indicator shows, in precedence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabStatus {
    /// The process ended (`clean` = exit code 0).
    Exited { clean: bool },
    /// omp waits for the user (`π !`).
    Attention,
    /// omp is working (spinner frame in the title).
    Busy,
    /// A bell rang on a background tab.
    Bell,
    /// Nothing to report: the caller's own idle look.
    Idle,
}

impl TabStatus {
    pub fn of(exit_code: Option<i32>, activity: Activity, bell: bool) -> TabStatus {
        match (exit_code, activity) {
            (Some(code), _) => TabStatus::Exited { clean: code == 0 },
            (None, Activity::Attention) => TabStatus::Attention,
            (None, Activity::Busy) => TabStatus::Busy,
            _ if bell => TabStatus::Bell,
            _ => TabStatus::Idle,
        }
    }

    /// Tooltip line for the live omp states.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            TabStatus::Attention => Some("omp is waiting for your input"),
            TabStatus::Busy => Some("omp is working…"),
            _ => None,
        }
    }
}

/// Paints `status` centred at `c` in an icon slot of `size` px; returns false for
/// [`TabStatus::Idle`], which the caller paints itself. Shared by the tab strip and the sidebar.
pub fn paint_status(ui: &Ui, c: Pos2, size: f32, status: TabStatus) -> bool {
    let pal = Palette::of_ui(ui);
    let p = ui.painter();
    match status {
        TabStatus::Exited { clean } => {
            let (icon, col) = if clean {
                (ICON_CIRCLE_CHECK, pal.ok)
            } else {
                (ICON_CIRCLE_X, pal.danger)
            };
            paint_icon(p, c, Align2::CENTER_CENTER, icon, size, col);
        }
        TabStatus::Attention => paint_attention_badge(p, pal, c, size),
        TabStatus::Busy => crate::widgets::Spin::new(size * 0.8, pal.accent_b).paint_at(ui, c),
        TabStatus::Bell => theme::glow_dot(p, c, size * 0.22, pal.warn, theme::GLOW_REST),
        TabStatus::Idle => return false,
    }
    true
}

/// Solid warn disc with a drawn `!` (font independent, legible at a glance).
fn paint_attention_badge(p: &Painter, pal: &Palette, c: Pos2, size: f32) {
    let r = size * 0.44;
    theme::glow_dot(p, c, r, pal.warn, theme::GLOW_REST);
    let ink = Stroke::new((size * 0.12).max(1.6), pal.bg);
    p.line_segment([c + vec2(0.0, -r * 0.5), c + vec2(0.0, r * 0.12)], ink);
    p.circle_filled(c + vec2(0.0, r * 0.5), ink.width * 0.6, pal.bg);
}

/// Warn wash and outline over a row/pill whose tab waits for the user.
pub fn paint_attention_pill(p: &Painter, pal: &Palette, rect: Rect, radius: u8) {
    p.rect(
        rect,
        radius,
        pal.warn.gamma_multiply(0.13),
        Stroke::new(1.0, pal.warn.gamma_multiply(0.7)),
        StrokeKind::Inside,
    );
}

/// What to relaunch when a closed tab is reopened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedTab {
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
}

/// Where a relaunched tab goes, remembered as its neighbours' uids (not an index): tabs that
/// close or open while the relaunch is prepared do not displace it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    /// The nearest terminal tab on the left: the new tab goes right after it.
    pub after: Option<u64>,
    /// The nearest terminal tab on the right: used when the left neighbour is gone.
    pub before: Option<u64>,
}

impl Anchor {
    /// The anchor of strip position `slot`; `uids` lists the strip (`None` = not a terminal tab).
    pub fn at(uids: &[Option<u64>], slot: usize) -> Anchor {
        let slot = slot.min(uids.len());
        Anchor {
            after: uids[..slot].iter().rev().find_map(|u| *u),
            before: uids[slot..].iter().find_map(|u| *u),
        }
    }

    /// The strip position to insert at: after the left neighbour, else before the right one,
    /// else at the end.
    pub fn index(&self, uids: &[Option<u64>]) -> usize {
        let find = |want: Option<u64>| want.and_then(|w| uids.iter().position(|u| *u == Some(w)));
        match (find(self.after), find(self.before)) {
            (Some(i), _) => i + 1,
            (None, Some(j)) => j,
            (None, None) => uids.len(),
        }
    }
}

/// Bounded LIFO of closed tabs.
#[derive(Debug)]
pub struct ReopenStack<T> {
    items: Vec<T>,
    cap: usize,
}

impl<T> ReopenStack<T> {
    pub fn new(cap: usize) -> Self {
        ReopenStack {
            items: Vec::new(),
            cap,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.cap {
            self.items.remove(0);
        }
        self.items.push(item);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }

    /// Drop every entry `keep` rejects.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.items.retain(keep);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Index after moving `delta` (±1) from `cur`, wrapping around `len`. `len == 0` → 0.
pub fn wrap_index(cur: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as isize + delta).rem_euclid(len as isize) as usize
}

/// `omp` or `omp --resume <abs path>`.
pub fn omp_argv(session_path: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["omp".to_owned()];
    if let Some(p) = session_path {
        argv.push("--resume".to_owned());
        argv.push(p.to_string_lossy().into_owned());
    }
    argv
}

/// The session file a tab should switch to given the files its tmux session has open: the first one
/// for a tab that knows none yet, or the only one when omp moved on (`/new`, `/fork`, `/resume`).
pub fn followed_path<'a>(current: Option<&Path>, files: &'a [PathBuf]) -> Option<&'a PathBuf> {
    match (current, files) {
        (None, [first, ..]) => Some(first),
        (Some(cur), [only]) if only != cur => Some(only),
        _ => None,
    }
}

/// Active index after removing tab `removed` from a list that had `len_before` tabs.
pub fn active_after_close(active: usize, removed: usize, len_before: usize) -> usize {
    let len = len_before.saturating_sub(1);
    let adjusted = if removed < active { active - 1 } else { active };
    adjusted.min(len.saturating_sub(1))
}

#[derive(Debug)]
pub enum TabAction {
    Activate(usize),
    Close(usize),
    /// Picked from the tab's context menu (the same menu as its "Open tabs" sidebar row).
    Menu(SidebarAction),
}

pub struct TabBarInput<'a> {
    pub tabs: &'a [Tab],
    pub labels: &'a HashMap<PathBuf, String>,
    pub active: usize,
    pub scroll_to_active: bool,
    pub menu: MenuEnv<'a>,
}

/// Height of a tab's hit area (the full bar height); the pill is painted inset.
pub fn tab_hit_h() -> f32 {
    theme::dens(32.0, 36.0)
}

fn pill_inset_y() -> f32 {
    if theme::neutral() { 3.0 } else { 4.0 }
}
const MAX_TEXT_W: f32 = 170.0;
const CLOSE_W: f32 = 20.0;
const ICON_W: f32 = 16.0;

/// Draws the tab strip (horizontal, scrollable); returns the requested action (at most one per frame).
/// `fade` is the bar background at the strip's left and right ends (for the overflow fades).
pub fn show_tab_strip(
    ui: &mut Ui,
    input: &TabBarInput<'_>,
    fade: (egui::Color32, egui::Color32),
) -> Option<TabAction> {
    let mut action = None;
    let strip = ui.available_rect_before_wrap();
    let hover_strip = ui.rect_contains_pointer(strip);
    let out = egui::ScrollArea::horizontal()
        .id_salt("tab-strip")
        .auto_shrink([true, false])
        // Never wider than the room left of the right-hand buttons, however narrow the window.
        .max_width(strip.width().max(0.0))
        .min_scrolled_width(0.0)
        .scroll_source(egui::scroll_area::ScrollSource {
            scroll_bar: false,
            drag: egui::scroll_area::DragScroll::Never,
            mouse_wheel: true,
        })
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            if hover_strip {
                // A plain vertical wheel scrolls the strip sideways.
                let dy = ui.input(|i| i.smooth_scroll_delta.y);
                if dy != 0.0 {
                    ui.scroll_with_delta(vec2(dy, 0.0));
                }
            }
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                for (i, tab) in input.tabs.iter().enumerate() {
                    if let Some(a) = show_tab(ui, input, i, tab)
                        && action.is_none()
                    {
                        action = Some(a);
                    }
                }
            });
        });
    let view = out.inner_rect;
    let max_offset = (out.content_size.x - view.width()).max(0.0);
    if max_offset > 0.5 {
        let offset = out.state.clamped_offset().x;
        let p = ui.painter();
        let edge = vec2(26.0_f32.min(view.width() / 2.0), view.height());
        if offset > 0.5 {
            let r = egui::Rect::from_min_size(view.min, edge);
            theme::gradient_rect(p, r, fade.0, fade.0.gamma_multiply(0.0), true);
        }
        if offset < max_offset - 0.5 {
            let r = egui::Rect::from_min_size(pos2(view.right() - edge.x, view.top()), edge);
            theme::gradient_rect(p, r, fade.1.gamma_multiply(0.0), fade.1, true);
        }
    }
    action
}

fn show_tab(ui: &mut Ui, input: &TabBarInput<'_>, i: usize, tab: &Tab) -> Option<TabAction> {
    let pal = Palette::of_ui(ui);
    let title = tab.label(input.labels);
    let active = i == input.active;
    let term = tab.term();
    let status = tab.status(active);

    let text_color = if active { pal.text_strong } else { pal.text };
    let mut job = egui::text::LayoutJob::simple_singleline(
        title.to_owned(),
        crate::theme::font(13.5),
        text_color,
    );
    job.wrap.max_width = MAX_TEXT_W;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);

    let width = 12.0 + ICON_W + 8.0 + galley.size().x + 6.0 + CLOSE_W + 6.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, tab_hit_h()), Sense::click());
    if input.scroll_to_active && active {
        ui.scroll_to_rect(rect.expand2(vec2(12.0, 0.0)), None);
    }
    let pill = rect.shrink2(vec2(0.0, pill_inset_y()));
    let close_rect = egui::Rect::from_center_size(
        pos2(pill.right() - 6.0 - CLOSE_W / 2.0, pill.center().y),
        vec2(CLOSE_W, CLOSE_W),
    );
    let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());

    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), active, 0.18);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        // Hover / active pill.
        let neutral = theme::neutral();
        let radius = if neutral { 6 } else { 10 };
        p.rect_filled(
            pill,
            radius,
            theme::hover_fill(pal).gamma_multiply(0.8 * hov * (1.0 - sel)),
        );
        if sel > 0.01 {
            p.rect(
                pill,
                radius,
                pal.surface.gamma_multiply(sel),
                Stroke::new(1.0, pal.stroke.gamma_multiply(sel)),
                StrokeKind::Inside,
            );
            if neutral {
                // Flat accent indicator along the pill's bottom edge.
                let line = egui::Rect::from_min_max(
                    pos2(pill.left() + 10.0, pill.bottom() - 2.0),
                    pos2(pill.right() - 10.0, pill.bottom()),
                );
                p.rect_filled(line, 1, pal.accent.gamma_multiply(sel));
            } else {
                // Glowing accent underline.
                let line = egui::Rect::from_min_max(
                    pos2(pill.left() + 12.0, pill.bottom() - 3.0),
                    pos2(pill.right() - 12.0, pill.bottom() - 1.0),
                );
                let glow = egui::Shadow {
                    offset: [0, 0],
                    blur: 12,
                    spread: 0,
                    color: pal.accent_a.gamma_multiply(0.55 * sel),
                };
                p.add(glow.as_shape(line, egui::CornerRadius::same(1)));
                theme::gradient_rounded(
                    p,
                    line,
                    1.0,
                    pal.accent_a.gamma_multiply(sel),
                    pal.accent_b.gamma_multiply(sel),
                );
            }
        }
        if status == TabStatus::Attention {
            paint_attention_pill(p, pal, pill, radius);
        }
        // Leading icon / status.
        let ic = pos2(pill.left() + 12.0 + ICON_W / 2.0, pill.center().y);
        if !paint_status(ui, ic, ICON_W, status) {
            let (icon, col) = tab.kind_icon(pal, sel);
            paint_icon(p, ic, Align2::CENTER_CENTER, icon, 16.0, col);
        }
        p.galley(
            pos2(
                pill.left() + 12.0 + ICON_W + 8.0,
                pill.center().y - galley.size().y / 2.0,
            ),
            galley,
            text_color,
        );
        // Close ✕: visible on hover and for the active tab.
        let vis = hov.max(sel * 0.8);
        if vis > 0.01 {
            let ch = ctx.animate_bool_with_time(close.id.with("hov"), close.hovered(), 0.1);
            p.circle_filled(
                close_rect.center(),
                CLOSE_W / 2.0,
                theme::hover_fill(pal).gamma_multiply(ch),
            );
            paint_icon(
                p,
                close_rect.center(),
                Align2::CENTER_CENTER,
                ICON_X,
                14.0,
                theme::mix(pal.weak, pal.text_strong, ch).gamma_multiply(vis),
            );
        }
    }

    let resp = resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_ui(|ui| {
            ui.label(title);
            if let Some(hint) = status.hint() {
                ui.weak(hint);
            }
            if let Some(p) = tab.session_path() {
                ui.weak(p.display().to_string());
            }
            if let Some(code) = term.and_then(|t| t.term.exit_code()) {
                ui.weak(format!("process exited: {code}"));
            }
        });

    let mut action = None;
    if close.clicked() || (resp.middle_clicked() && !close.hovered()) {
        action = Some(TabAction::Close(i));
    } else if resp.clicked() && !close.hovered() {
        action = Some(TabAction::Activate(i));
    }
    theme::context_menu(&resp, 230.0, |ui| {
        let mut out = SidebarOutput::default();
        sidebar::tab_menu(ui, i, tab, &input.menu, &mut out);
        if let Some(a) = out.actions.pop() {
            action = Some(TabAction::Menu(a));
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_survives_a_tab_closing_in_front_of_it() {
        // Strip A B [slot] C D; the tab at the slot is being relaunched.
        let strip = [Some(1), Some(2), Some(3), Some(4)];
        let anchor = Anchor::at(&strip, 2);
        assert_eq!(
            anchor,
            Anchor {
                after: Some(2),
                before: Some(3)
            }
        );
        assert_eq!(anchor.index(&strip), 2);
        // A closes meanwhile: an absolute index 2 would now land after C.
        let closed_front = [Some(2), Some(3), Some(4)];
        assert_eq!(anchor.index(&closed_front), 1);
        // The left neighbour closes: fall back to the right one.
        assert_eq!(anchor.index(&[Some(1), Some(3), Some(4)]), 1);
        // Both neighbours gone: the end.
        assert_eq!(anchor.index(&[Some(1), Some(4)]), 2);
        // Edges and non-terminal tabs.
        assert_eq!(Anchor::at(&[None, Some(7)], 1).after, None);
        assert_eq!(
            Anchor::at(&[Some(7)], 0),
            Anchor {
                after: None,
                before: Some(7)
            }
        );
        assert_eq!(Anchor::at(&[], 0).index(&[]), 0);
    }

    #[test]
    fn a_tab_follows_omp_to_its_only_open_session_file() {
        let (a, b) = (PathBuf::from("/s/a.jsonl"), PathBuf::from("/s/b.jsonl"));
        assert_eq!(followed_path(None, std::slice::from_ref(&a)), Some(&a));
        assert_eq!(followed_path(Some(&a), std::slice::from_ref(&b)), Some(&b));
        assert_eq!(followed_path(Some(&a), std::slice::from_ref(&a)), None);
        assert_eq!(followed_path(Some(&a), &[]), None);
        // Several files open (a subagent, a pending fork): no guess.
        assert_eq!(followed_path(Some(&a), &[a.clone(), b.clone()]), None);
        assert_eq!(followed_path(Some(&a), &[b.clone(), a.clone()]), None);
    }

    #[test]
    fn wrap_switching() {
        assert_eq!(wrap_index(0, 3, 1), 1);
        assert_eq!(wrap_index(2, 3, 1), 0);
        assert_eq!(wrap_index(0, 3, -1), 2);
        assert_eq!(wrap_index(1, 3, -1), 0);
        assert_eq!(wrap_index(0, 1, 1), 0);
        assert_eq!(wrap_index(0, 0, 1), 0);
    }

    #[test]
    fn reopen_stack_lifo_and_cap() {
        let mut s = ReopenStack::new(2);
        assert!(s.is_empty());
        s.push(1);
        s.push(2);
        s.push(3);
        assert_eq!(s.pop(), Some(3));
        assert_eq!(s.pop(), Some(2));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn argv_forms() {
        assert_eq!(omp_argv(None), ["omp"]);
        assert_eq!(
            omp_argv(Some(Path::new("/a/b.jsonl"))),
            ["omp", "--resume", "/a/b.jsonl"]
        );
    }

    #[test]
    fn active_index_after_close() {
        assert_eq!(active_after_close(1, 0, 3), 0); // closed before active
        assert_eq!(active_after_close(1, 2, 3), 1); // closed after active
        assert_eq!(active_after_close(1, 1, 3), 1); // closed active, next slides in
        assert_eq!(active_after_close(2, 2, 3), 1); // closed last active
        assert_eq!(active_after_close(0, 0, 1), 0); // empty: clamped to 0
    }

    #[test]
    fn tab_status_precedence() {
        use Activity::*;
        let of = TabStatus::of;
        // Exited wins over everything; the code decides clean/failed.
        assert_eq!(
            of(Some(0), Attention, true),
            TabStatus::Exited { clean: true }
        );
        assert_eq!(of(Some(1), Busy, false), TabStatus::Exited { clean: false });
        // Then attention, busy, the bell, idle.
        assert_eq!(of(None, Attention, true), TabStatus::Attention);
        assert_eq!(of(None, Busy, true), TabStatus::Busy);
        assert_eq!(of(None, Idle, true), TabStatus::Bell);
        assert_eq!(of(None, Unknown, true), TabStatus::Bell);
        assert_eq!(of(None, Idle, false), TabStatus::Idle);
        assert_eq!(of(None, Unknown, false), TabStatus::Idle);
    }
}
