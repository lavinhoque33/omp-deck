//! The bottom status bar: what omp's own status line shows for the active session (activity,
//! model + thinking, path, git, context meter, tokens, cost) plus the session action buttons.
//!
//! Strings are formatted once per status snapshot ([`BarView::update`]); drawing only paints.

use std::path::Path;
use std::sync::Arc;

use deck_core::omp::ModelInfo;
use deck_core::status::{GitInfo, SessionStatus};
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Layout, Margin, Painter, Pos2, Rect, Response,
    RichText, Sense, Shape, Stroke, Ui, UiBuilder, pos2, vec2,
};
use egui_material_icons::icons::{
    ICON_ACCOUNT_TREE, ICON_ALT_ROUTE, ICON_AUTO_AWESOME, ICON_CHECK_CIRCLE, ICON_COMPRESS,
    ICON_CONTENT_COPY, ICON_DONUT_LARGE, ICON_DOWNLOAD, ICON_EDIT, ICON_ERROR, ICON_FOLDER,
    ICON_FORK_RIGHT, ICON_HANDSHAKE, ICON_HUB, ICON_MAP, ICON_MORE_HORIZ, ICON_REPLAY, ICON_STOP,
    ICON_STOP_CIRCLE, ICON_TERMINAL, ICON_UNFOLD_MORE,
};

use crate::agent_model::Counts;

use crate::model_picker::{self, ModelsCache, Pending, PickerAction, PickerInput, PickerState};
use crate::session_actions::{self, Action, ActionCtx, Availability, CompactMode, Kind};
use crate::statusfmt::{
    self, Activity, ContextLevel, context_label, context_level, context_percent, fmt_cost,
    fmt_tokens, thinking_fill, thinking_short,
};
use crate::theme::{self, Palette};
use crate::widgets::{icon_text, menu_item, paint_icon};

pub const BAR_H: f32 = 41.0;
const CHIP_H: f32 = 29.0;
const PATH_CHARS: usize = 44;
const CHIP_PAD: f32 = 7.0;
/// Horizontal space a chip adds around its content: both pads plus the one item gap measured
/// on screen (so a chip asked to be N px wide really is N px wide).
const CHIP_CHROME_W: f32 = 2.0 * CHIP_PAD + 6.0;
/// The model chip is 1.3x the wider of the measured path chip and this baseline width.
const MODEL_CHIP_BASE_W: f32 = 213.0;
const MODEL_CHIP_SCALE: f32 = 1.3;
const PIE_BOX: f32 = 18.0;
const CARET_W: f32 = 16.8;

// ------------------------------------------------------------------ view (cached strings)

pub struct GitView {
    pub branch: String,
    pub dirty: bool,
    /// `↑2 ↓1`
    pub sync: String,
    pub unstaged: String,
    pub staged: String,
    pub untracked: String,
    pub tooltip: String,
    pub pr: Option<(String, String)>,
}

/// Everything the bar shows, formatted. Rebuilt when the snapshot or the model catalog changes.
#[derive(Default)]
pub struct BarView {
    defaults: Defaults,
    snap: Option<Arc<SessionStatus>>,
    catalog_gen: u64,
    built: bool,
    /// Bumps on every rebuild (invalidates cached segment widths).
    pub rev: u64,

    pub model_name: String,
    pub model_selector: Option<String>,
    /// `model_selector`/`level` come from omp's settings (the session has not recorded a model
    /// yet): shown dimmed with a tooltip, replaced by the first recorded model.
    pub model_is_default: bool,
    /// Effective thinking level (None = the model has none / unknown).
    pub level: Option<String>,
    pub configured: Option<String>,
    pub model_tip: String,

    pub path_text: String,
    pub path_full: String,

    pub git: Option<GitView>,

    pub ctx_label: String,
    pub ctx_frac: Option<f32>,
    pub ctx_level: Option<ContextLevel>,
    pub ctx_tip: String,

    pub tokens_in: String,
    pub tokens_out: String,
    pub tokens_tip: String,
    pub show_tokens: bool,
    pub cost: String,
    pub cost_tip: String,
    pub show_cost: bool,
    pub last_turn_failed: bool,
    pub has_reply: bool,
}

/// omp's configured default model and thinking level (`modelRoles.default`,
/// `defaultThinkingLevel`), shown for a fresh session that has no `model_change` entry yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub model: Option<String>,
    pub level: Option<String>,
}

impl BarView {
    /// New defaults invalidate the built view (rebuilt by the next [`BarView::update`]).
    pub fn set_defaults(&mut self, defaults: &Defaults) {
        if self.defaults != *defaults {
            self.defaults = defaults.clone();
            self.built = false;
        }
    }

    /// Rebuild when the snapshot (by identity) or the catalog changed; true when it did.
    pub fn update(
        &mut self,
        snap: Option<&Arc<SessionStatus>>,
        fallback_cwd: &Path,
        catalog: &ModelsCache,
    ) -> bool {
        let same_snap = match (&self.snap, snap) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if self.built && same_snap && self.catalog_gen == catalog.generation {
            return false;
        }
        self.snap = snap.cloned();
        self.rev += 1;
        self.catalog_gen = catalog.generation;
        self.built = true;
        match snap {
            Some(s) => self.build(s, catalog),
            None => self.build(
                &SessionStatus {
                    cwd: fallback_cwd.to_path_buf(),
                    ..SessionStatus::default()
                },
                catalog,
            ),
        }
        true
    }

    fn build(&mut self, s: &SessionStatus, catalog: &ModelsCache) {
        let st = &s.stats;
        self.model_is_default = st.model.is_none() && self.defaults.model.is_some();
        let selector = if self.model_is_default {
            self.defaults.model.clone()
        } else {
            st.model.clone()
        };
        let info: Option<&ModelInfo> = selector.as_deref().and_then(|m| catalog.get(m));
        self.model_selector = selector.clone();
        self.model_name = match (info, selector.as_deref()) {
            (Some(i), _) => i.name.clone(),
            (None, Some(sel)) => sel.rsplit('/').next().unwrap_or(sel).to_owned(),
            (None, None) => String::new(),
        };
        if self.model_is_default {
            // A model without thinking levels ignores the default level.
            let level = self
                .defaults
                .level
                .clone()
                .filter(|_| info.is_none_or(|i| !i.thinking.is_empty()));
            self.level = level.clone();
            self.configured = level;
        } else {
            self.level = st.thinking.clone();
            self.configured = st.thinking_configured.clone();
        }
        self.model_tip = model_tooltip(
            selector.as_deref(),
            info,
            self.level.as_deref(),
            self.configured.as_deref(),
            self.model_is_default,
        );

        self.path_full = s.cwd.to_string_lossy().into_owned();
        self.path_text = statusfmt::display_path(&s.cwd, PATH_CHARS);

        self.git = s.git.as_ref().map(git_view);

        let window = info.and_then(|i| i.context_window);
        self.ctx_label = context_label(st.context_tokens, window);
        let pct = context_percent(st.context_tokens, window);
        self.ctx_frac = pct.map(|p| (p / 100.0).clamp(0.0, 1.0) as f32);
        self.ctx_level = pct.zip(window).map(|(p, w)| context_level(p, w));
        self.ctx_tip = context_tooltip(st.context_tokens, window, pct);

        self.tokens_in = fmt_tokens(st.input_tokens);
        self.tokens_out = fmt_tokens(st.output_tokens);
        self.show_tokens = st.input_tokens + st.output_tokens + st.cache_read + st.cache_write > 0;
        self.tokens_tip = tokens_tooltip(
            st.input_tokens,
            st.output_tokens,
            st.cache_read,
            st.cache_write,
        );
        self.cost = fmt_cost(st.cost);
        self.show_cost = st.cost > 0.0;
        self.last_turn_failed = st.last_turn_failed;
        self.has_reply = st.last_assistant_text.is_some();
        self.cost_tip = format!(
            "Session cost ${:.4}\nSum of every assistant reply and side-model call in this session file",
            st.cost
        );
    }

    /// Selector + level the dropdown highlights.
    pub fn current_level(&self) -> Option<&str> {
        self.configured.as_deref().or(self.level.as_deref())
    }
}

fn git_view(g: &GitInfo) -> GitView {
    let sync = match (g.ahead, g.behind) {
        (0, 0) => String::new(),
        (a, 0) => format!("↑{a}"),
        (0, b) => format!("↓{b}"),
        (a, b) => format!("↑{a} ↓{b}"),
    };
    let count = |prefix: char, n: u32| {
        if n > 0 {
            format!("{prefix}{n}")
        } else {
            String::new()
        }
    };
    let mut parts = vec![format!("Branch {}", g.branch)];
    if g.ahead > 0 || g.behind > 0 {
        parts.push(format!("{} ahead, {} behind upstream", g.ahead, g.behind));
    }
    for (n, what) in [
        (g.staged, "staged"),
        (g.unstaged, "modified (unstaged)"),
        (g.untracked, "untracked"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    if parts.len() == 1 {
        parts.push("Working tree clean".to_owned());
    }
    GitView {
        branch: g.branch.clone(),
        dirty: g.staged + g.unstaged + g.untracked > 0,
        sync,
        unstaged: count('*', g.unstaged),
        staged: count('+', g.staged),
        untracked: count('?', g.untracked),
        tooltip: parts.join("\n"),
        pr: g
            .pr
            .as_ref()
            .map(|p| (format!("#{}", p.number), p.url.clone())),
    }
}

/// `1234567` → `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn model_tooltip(
    selector: Option<&str>,
    info: Option<&ModelInfo>,
    level: Option<&str>,
    configured: Option<&str>,
    is_default: bool,
) -> String {
    let mut t = String::new();
    if is_default {
        t.push_str("Default model (from omp's settings)\n");
    }
    match (selector, info) {
        (Some(sel), Some(i)) => {
            t.push_str(&format!("{}\n{sel}", i.name));
        }
        (Some(sel), None) => t.push_str(sel),
        _ => t.push_str("Model not recorded yet"),
    }
    match (configured, level) {
        (Some("auto"), Some(l)) => t.push_str(&format!("\nThinking: auto (currently {l})")),
        (Some("auto"), None) => t.push_str("\nThinking: auto"),
        (_, Some(l)) => t.push_str(&format!("\nThinking: {l}")),
        _ => {}
    }
    if is_default {
        t.push_str("\nThe session has not used a model yet.");
    }
    t.push_str("\n\nClick to switch model or thinking level");
    t
}

fn context_tooltip(tokens: Option<u64>, window: Option<u64>, pct: Option<f64>) -> String {
    match (tokens, window, pct) {
        (Some(t), Some(w), Some(p)) => format!(
            "Context window: {} of {} tokens ({p:.1}%)\nPrompt tokens of the last reply; omp also counts text added since",
            group_digits(t),
            group_digits(w)
        ),
        (Some(t), None, _) => format!(
            "Context: {} tokens\nThe model's window size is unknown",
            group_digits(t)
        ),
        (None, Some(w), _) => format!("Context window: {} tokens\nNo reply yet", group_digits(w)),
        _ => "Context usage appears after the first reply".to_owned(),
    }
}

fn tokens_tooltip(input: u64, output: u64, cache_read: u64, cache_write: u64) -> String {
    format!(
        "Tokens for the whole session\n↑ input {}\n↓ output {}\ncache read {}\ncache write {}",
        group_digits(input),
        group_digits(output),
        group_digits(cache_read),
        group_digits(cache_write)
    )
}

// ------------------------------------------------------------------ actions out

#[derive(Debug, PartialEq, Eq)]
pub enum BarAction {
    Picker(PickerAction),
    Run(Action),
    AskHandoff,
    AskCompactFocus,
    AskRename,
    CopyPath(String),
    OpenUrl(String),
    ToggleAgentHub,
    OpenTree,
}

pub struct BarInput<'a> {
    pub catalog: &'a ModelsCache,
    pub favorites: &'a [String],
    pub provider: Option<&'a str>,
    pub activity: Activity,
    /// Optimistic switch awaiting confirmation.
    pub pending: Option<&'a Pending>,
    /// Last accepted switch of a session file that does not record models yet.
    pub assumed: Option<&'a Pending>,
    pub actions: ActionCtx,
    pub agents: AgentsChip,
}

/// What the status bar's "Agents" chip shows.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentsChip {
    pub counts: Counts,
    /// The hub panel is open.
    pub open: bool,
}

// ------------------------------------------------------------------ drawing

/// Draws the bar as a bottom panel and returns what the user asked for.
pub fn show(
    ui: &mut Ui,
    view: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
) -> Vec<BarAction> {
    let pal = Palette::of_ui(ui);
    let mut out = Vec::new();
    let panel = egui::Panel::bottom("status-bar")
        .resizable(false)
        .exact_size(BAR_H)
        .show_separator_line(false)
        .frame(
            Frame::NONE
                .fill(pal.bar)
                .inner_margin(Margin::symmetric(12, 0)),
        )
        .show(ui, |ui| {
            let full = ui.max_rect();
            let right = ui.scope_builder(
                UiBuilder::new()
                    .max_rect(full)
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| actions_row(ui, input, &mut out),
            );
            let left_max = (right.response.rect.left() - 8.0).max(full.left());
            let left_rect = Rect::from_min_max(full.min, pos2(left_max, full.max.y));
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(left_rect)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.set_clip_rect(left_rect.intersect(ui.clip_rect()));
                    info_row(ui, view, picker, input, &mut out);
                },
            );
        });
    let r = panel.response.rect;
    ui.painter().hline(
        r.x_range(),
        r.top() + 0.5,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
    );
    out
}

/// The droppable segments of the info row, most important first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seg {
    Ctx,
    Git,
    Path,
    Cost,
    Tokens,
}

const SEGS: [Seg; 5] = [Seg::Ctx, Seg::Git, Seg::Path, Seg::Cost, Seg::Tokens];

/// Real widths (divider + chip, without the spacing before them) of every segment for one view
/// revision and pixel density.
#[derive(Clone, Copy)]
struct SegWidths {
    key: (u64, u32),
    w: [f32; 5],
}

/// Lays `f` out once in an invisible child and returns the width it took.
fn measure(ui: &mut Ui, f: impl FnOnce(&mut Ui)) -> f32 {
    let r = ui.max_rect();
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("sb-measure")
            .max_rect(Rect::from_min_size(r.min, vec2(4000.0, r.height())))
            .layout(Layout::left_to_right(Align::Center))
            .invisible(),
    );
    f(&mut child);
    child.min_rect().width()
}

fn seg_widths(ui: &mut Ui, v: &BarView) -> [f32; 5] {
    let key = (v.rev, ui.ctx().pixels_per_point().to_bits());
    let id = ui.scope_id().with("seg-widths");
    if let Some(c) = ui.data(|d| d.get_temp::<SegWidths>(id))
        && c.key == key
    {
        return c.w;
    }
    let mut w = [0.0; 5];
    let mut sink = Vec::new();
    for (i, s) in SEGS.into_iter().enumerate() {
        w[i] = measure(ui, |ui| segment(ui, v, s, &mut sink));
    }
    ui.data_mut(|d| d.insert_temp(id, SegWidths { key, w }));
    w
}

fn info_row(
    ui: &mut Ui,
    v: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
    out: &mut Vec<BarAction>,
) {
    ui.spacing_mut().item_spacing.x = 6.0;

    activity_dot(ui, input.activity);
    ui.add_space(3.0);
    // The model chip is as wide as the (measured) path chip, but never takes over the bar.
    let widths = seg_widths(ui, v);
    let gap = ui.spacing().item_spacing.x;
    let (chip_w, cap) = model_chip_width(widths[2] - gap - 1.0, ui.available_width());
    model_chip(ui, v, picker, input, (chip_w, cap), out);

    // Priority-based fit on measured widths: drop the least important segments first.
    let present = SEGS.map(|s| match s {
        Seg::Git => v.git.is_some(),
        Seg::Cost => v.show_cost,
        Seg::Tokens => v.show_tokens,
        Seg::Ctx | Seg::Path => true,
    });
    let shown = fit_segments(&widths, &present, ui.available_width() - gap, gap);
    // Drawn in reading order: path, git, ctx, tokens, cost.
    for s in [Seg::Path, Seg::Git, Seg::Ctx, Seg::Tokens, Seg::Cost] {
        if shown[SEGS.iter().position(|x| *x == s).unwrap_or(0)] {
            segment(ui, v, s, out);
        }
    }
}

/// Model chip `(width, cap)`: 1.3x the wider of the path chip and the baseline, capped at 52% of
/// the bar so it never takes over a narrow bar.
fn model_chip_width(path_chip_w: f32, bar_w: f32) -> (f32, f32) {
    let cap = (bar_w * 0.52).max(195.0);
    let want = path_chip_w.max(MODEL_CHIP_BASE_W) * MODEL_CHIP_SCALE;
    (want.min(cap), cap)
}

/// Which segments fit `budget`, in priority order: the first present one that does not fit ends
/// the list, so a narrower bar never shows a segment a wider one dropped.
fn fit_segments(widths: &[f32; 5], present: &[bool; 5], mut budget: f32, gap: f32) -> [bool; 5] {
    let mut shown = [false; 5];
    for i in 0..SEGS.len() {
        if !present[i] {
            continue;
        }
        let cost = widths[i] + gap;
        if budget < cost {
            break;
        }
        budget -= cost;
        shown[i] = true;
    }
    shown
}

/// One segment with its leading divider.
fn segment(ui: &mut Ui, v: &BarView, seg: Seg, out: &mut Vec<BarAction>) {
    let pal = Palette::of_ui(ui);
    match seg {
        Seg::Path => {
            divider(ui);
            let r = chip(ui, "path", Sense::click(), |ui| {
                ui.label(icon_text(ICON_FOLDER, 14.4, pal.weak));
                ui.add(text(&v.path_text, 14.4, pal.text));
            });
            if r.on_hover_text(format!("{}\nClick to copy", v.path_full))
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                out.push(BarAction::CopyPath(v.path_full.clone()));
            }
        }
        Seg::Git => {
            if let Some(g) = &v.git {
                divider(ui);
                git_chip(ui, g, out);
            }
        }
        Seg::Ctx => {
            divider(ui);
            context_meter(ui, v);
        }
        Seg::Tokens => {
            divider(ui);
            chip(ui, "tokens", Sense::hover(), |ui| {
                ui.add(text("↑", 14.4, pal.weak));
                ui.add(text(&v.tokens_in, 14.4, pal.text));
                ui.add_space(4.0);
                ui.add(text("↓", 14.4, pal.weak));
                ui.add(text(&v.tokens_out, 14.4, pal.text));
            })
            .on_hover_text(&v.tokens_tip);
        }
        Seg::Cost => {
            divider(ui);
            chip(ui, "cost", Sense::hover(), |ui| {
                ui.label(RichText::new(&v.cost).size(14.4).color(pal.ok));
            })
            .on_hover_text(&v.cost_tip);
        }
    }
}

fn text(s: &str, size: f32, color: Color32) -> egui::Label {
    egui::Label::new(RichText::new(s).size(size).color(color)).selectable(false)
}

/// A rounded segment with a hover plate; `add` lays out its contents on one line. The response
/// is registered after the contents, so it wins hit-testing and labels never swallow clicks.
fn chip(ui: &mut Ui, id: &'static str, sense: Sense, add: impl FnOnce(&mut Ui)) -> Response {
    let pal = Palette::of_ui(ui);
    let bg = ui.painter().add(Shape::Noop);
    let rect = ui
        .horizontal(|ui| {
            ui.set_height(CHIP_H);
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add_space(CHIP_PAD);
            add(ui);
            ui.add_space(CHIP_PAD);
        })
        .response
        .rect;
    let r = ui.interact(rect, ui.scope_id().with(("bar-chip", id)), sense);
    if r.clicked() {
        // Keep keyboard focus with the terminal, not the chip.
        r.surrender_focus();
    }
    if sense != Sense::hover() && r.hovered() {
        ui.painter().set(
            bg,
            Shape::rect_filled(r.rect, 10, pal.raised.gamma_multiply(0.85)),
        );
    }
    r
}

fn divider(ui: &mut Ui) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 16.8), Sense::hover());
    ui.painter()
        .rect_filled(rect, 0, pal.stroke.gamma_multiply(0.8));
}

fn activity_dot(ui: &mut Ui, activity: Activity) {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    let (tip, color) = match activity {
        Activity::Busy => {
            // Spinner only while a turn runs; it schedules its own repaints.
            let spinner = crate::widgets::Spin::new(15.6, pal.accent);
            ui.put(
                Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
                spinner,
            );
            return void_tip(resp, "omp is working");
        }
        Activity::Idle => ("omp is idle", pal.ok),
        Activity::Attention => ("omp needs your attention", pal.warn),
        Activity::Unknown => ("Waiting for omp…", pal.weak.gamma_multiply(0.6)),
    };
    if activity == Activity::Unknown {
        ui.painter()
            .circle_stroke(rect.center(), 4.2, Stroke::new(1.6, color));
    } else {
        theme::glow_dot(ui.painter(), rect.center(), 4.2, color, theme::GLOW_REST);
    }
    void_tip(resp, tip);
}

fn void_tip(resp: Response, tip: &str) {
    resp.on_hover_text(tip);
}

fn model_chip(
    ui: &mut Ui,
    v: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
    (min_w, max_w): (f32, f32),
    out: &mut Vec<BarAction>,
) {
    let pal = Palette::of_ui(ui);
    let pending = input.pending;
    let assumed = input
        .assumed
        .filter(|_| v.model_selector.is_none() || v.model_is_default);
    let resp = chip(ui, "model", Sense::click(), |ui| {
        if let Some(p) = pending {
            ui.add(crate::widgets::Spin::new(16.8, pal.accent));
            ui.add(text(
                &format!("switching to {}…", p.describe(input.catalog)),
                15.2,
                pal.accent,
            ));
            return;
        }
        let name = match assumed {
            Some(a) => a.describe(input.catalog),
            None if v.model_name.is_empty() => "model".to_owned(),
            None => v.model_name.clone(),
        };
        let level = v
            .level
            .as_deref()
            .or(assumed.and_then(|a| a.level.as_deref()));
        let configured = v
            .configured
            .as_deref()
            .or(assumed.and_then(|a| a.level.as_deref()));
        let col = if (v.model_name.is_empty() || v.model_is_default) && assumed.is_none() {
            pal.weak
        } else {
            pal.text_strong
        };
        // Effort glyph + label and the caret sit at the right edge of a fixed-width chip; the
        // model name takes the rest and ellipsizes.
        let effort = configured.filter(|c| *c == "auto").or(level.or(configured));
        let label_w = effort.map_or(0.0, |l| {
            ui.painter()
                .layout_no_wrap(
                    thinking_short(l).to_owned(),
                    egui::FontId::proportional(14.4),
                    pal.accent,
                )
                .size()
                .x
        });
        let spacing = ui.spacing().item_spacing.x;
        let right_w = CARET_W + effort.map_or(0.0, |_| spacing + PIE_BOX + spacing + label_w);
        let name_text = name.as_str();
        let name_nat = ui
            .painter()
            .layout_no_wrap(name_text.to_owned(), egui::FontId::proportional(15.0), col)
            .size()
            .x
            + 8.0;
        let inner_w = (min_w - CHIP_CHROME_W)
            .max((name_nat + spacing + right_w).min(max_w - CHIP_CHROME_W))
            .min(ui.available_width())
            .max(0.0);
        let name_w = (inner_w - right_w - spacing).max(0.0);
        ui.allocate_ui_with_layout(
            vec2(name_w, CHIP_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                // allocate_ui_with_layout only keeps the content's rect: pin the floor.
                ui.set_min_size(vec2(name_w, CHIP_H));
                ui.add(
                    egui::Label::new(
                        RichText::new(name_text)
                            .size(15.0)
                            .color(col)
                            .variation("wght", 620.0),
                    )
                    .truncate()
                    .selectable(false),
                );
            },
        );
        if let Some(l) = effort {
            let (rect, _) = ui.allocate_exact_size(vec2(PIE_BOX, PIE_BOX), Sense::hover());
            paint_pie(
                ui.painter(),
                rect.center(),
                7.2,
                thinking_fill(l),
                pal.accent,
            );
            ui.add(text(thinking_short(l), 14.4, pal.accent));
        }
        ui.label(icon_text(ICON_UNFOLD_MORE, 16.8, pal.weak));
    });
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let resp = if egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp)) {
        resp
    } else {
        resp.on_hover_text(&v.model_tip)
    };
    let acts = model_picker::show(
        &resp,
        picker,
        &PickerInput {
            catalog: input.catalog,
            current: v.model_selector.as_deref(),
            level: v.current_level(),
            favorites: input.favorites,
            provider: input.provider,
        },
    );
    out.extend(acts.into_iter().map(BarAction::Picker));
}

/// Thinking glyph: ring + pie sector (clockwise from 12 o'clock); `None` = off (slashed ring).
fn paint_pie(p: &Painter, c: Pos2, r: f32, fill: Option<f32>, color: Color32) {
    let ring = Stroke::new(1.6, color);
    p.circle_stroke(c, r, ring);
    match fill {
        None => {
            let d = r * 0.7;
            p.line_segment([c + vec2(-d, d), c + vec2(d, -d)], ring);
        }
        Some(f) if f >= 0.995 => {
            p.circle_filled(c, r - 1.8, color);
        }
        Some(f) if f > 0.0 => {
            let steps = ((f * 24.0).ceil() as usize).max(2);
            let mut pts = Vec::with_capacity(steps + 2);
            pts.push(c);
            for i in 0..=steps {
                let a = -std::f32::consts::FRAC_PI_2
                    + f * std::f32::consts::TAU * i as f32 / steps as f32;
                pts.push(c + vec2(a.cos(), a.sin()) * (r - 1.8));
            }
            p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        _ => {}
    }
}

fn git_chip(ui: &mut Ui, g: &GitView, out: &mut Vec<BarAction>) {
    let pal = Palette::of_ui(ui);
    let r = chip(ui, "git", Sense::hover(), |ui| {
        ui.label(icon_text(
            ICON_ALT_ROUTE,
            18.0,
            if g.dirty { pal.warn } else { pal.ok },
        ));
        ui.add(text(&g.branch, 14.4, pal.text));
        if !g.sync.is_empty() {
            ui.add(text(&g.sync, 13.8, pal.weak));
        }
        if !g.unstaged.is_empty() {
            ui.add(text(&g.unstaged, 13.8, pal.warn));
        }
        if !g.staged.is_empty() {
            ui.add(text(&g.staged, 13.8, pal.ok));
        }
        if !g.untracked.is_empty() {
            ui.add(text(&g.untracked, 13.8, pal.weak));
        }
    });
    r.on_hover_text(&g.tooltip);
    if let Some((label, url)) = &g.pr {
        let pr = chip(ui, "pr", Sense::click(), |ui| {
            ui.add(text("PR", 13.2, pal.weak));
            ui.label(
                RichText::new(label)
                    .size(14.4)
                    .color(pal.accent)
                    .underline(),
            );
        });
        if pr
            .on_hover_text(format!("{url}\nClick to open"))
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            out.push(BarAction::OpenUrl(url.clone()));
        }
    }
}

fn context_meter(ui: &mut Ui, v: &BarView) {
    let pal = Palette::of_ui(ui);
    let color = match v.ctx_level {
        None | Some(ContextLevel::Normal) => pal.accent,
        Some(ContextLevel::Warn) => pal.warn,
        Some(ContextLevel::High) => theme::mix(pal.warn, pal.danger, 0.5),
        Some(ContextLevel::Critical) => pal.danger,
    };
    chip(ui, "ctx", Sense::hover(), |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(81.0, 16.8), Sense::hover());
        let bar = Rect::from_center_size(rect.center(), vec2(65.0, 5.0));
        let p = ui.painter();
        p.rect_filled(bar, 3, pal.surface);
        if let Some(f) = v.ctx_frac {
            let w = (bar.width() * f).max(if f > 0.0 { 3.6 } else { 0.0 });
            let fill = Rect::from_min_size(bar.min, vec2(w, bar.height()));
            p.rect_filled(fill, 3, color);
        }
        let label_color = if v.ctx_level.is_some_and(|l| l != ContextLevel::Normal) {
            color
        } else {
            pal.text
        };
        ui.add(text(&v.ctx_label, 14.4, label_color));
    })
    .on_hover_text(&v.ctx_tip);
}

/// The glyph of one count in the Agents chip.
enum Glyph {
    Spinner,
    Icon(egui_material_icons::MaterialIcon),
}

/// `⟳ 2 · ✓ 5 · ✕ 1` — live counts of the active session's subagents; click toggles the hub.
fn agents_chip(ui: &mut Ui, v: &AgentsChip, out: &mut Vec<BarAction>) {
    let pal = Palette::of_ui(ui);
    let c = v.counts;
    let total = c.total();
    // The action row lays out right → left and `horizontal` inherits that: contents are emitted
    // in reverse reading order (last count first, number before its glyph, hub icon last).
    let r = chip(ui, "agents", Sense::click(), |ui| {
        let lit = v.open || c.running > 0;
        let icon_color = if lit {
            pal.accent
        } else if total == 0 {
            pal.weak.gamma_multiply(0.7)
        } else {
            pal.weak
        };
        if total == 0 {
            ui.add(text("Agents", 14.4, pal.weak.gamma_multiply(0.85)));
        } else {
            let mut first = true;
            let mut seg = |ui: &mut Ui, n: usize, glyph: Glyph, color: Color32| {
                if n == 0 {
                    return;
                }
                if !first {
                    ui.add(text("·", 14.4, pal.weak));
                }
                first = false;
                ui.add(text(&n.to_string(), 14.4, pal.text));
                match glyph {
                    Glyph::Spinner => {
                        ui.add(crate::widgets::Spin::new(15.6, color));
                    }
                    Glyph::Icon(i) => {
                        ui.label(icon_text(i, 16.8, color));
                    }
                }
            };
            seg(
                ui,
                c.aborted + c.interrupted,
                Glyph::Icon(ICON_STOP_CIRCLE),
                pal.warn,
            );
            seg(ui, c.failed, Glyph::Icon(ICON_ERROR), pal.danger);
            seg(ui, c.done, Glyph::Icon(ICON_CHECK_CIRCLE), pal.ok);
            seg(ui, c.running, Glyph::Spinner, pal.accent_b);
        }
        ui.label(icon_text(ICON_HUB, 19.2, icon_color));
    });
    if v.open {
        let p = ui.painter();
        p.rect_filled(r.rect, 10, pal.accent.gamma_multiply(0.14));
        p.rect_stroke(
            r.rect,
            10,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
    }
    let r = r
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_ui(|ui| {
            ui.label(RichText::new("Agent Hub  (Ctrl+Shift+A)").strong());
            if total == 0 {
                ui.label("No subagents in this session yet");
            } else {
                let mut parts = Vec::new();
                for (n, what) in [
                    (c.running, "running"),
                    (c.done, "done"),
                    (c.failed, "failed"),
                    (c.aborted, "aborted"),
                    (c.interrupted, "interrupted"),
                ] {
                    if n > 0 {
                        parts.push(format!("{n} {what}"));
                    }
                }
                ui.label(parts.join(" · "));
            }
        });
    if r.clicked() {
        out.push(BarAction::ToggleAgentHub);
    }
}

// ------------------------------------------------------------------ action buttons

fn actions_row(ui: &mut Ui, input: &BarInput<'_>, out: &mut Vec<BarAction>) {
    let c = &input.actions;
    ui.spacing_mut().item_spacing.x = 3.0;

    // Added right → left: the Agents chip sits at the far right, under the hub panel.
    agents_chip(ui, &input.agents, out);
    ui.add_space(6.0);

    // Added right → left, so this reads: Stop/Retry · Plan · Handoff · Compact ▾ · Fork · Tree · ⋯
    let more = bar_button(ui, ICON_MORE_HORIZ, "More actions", true, None, false);
    theme::dropup_menu(&more, 220.0, |ui| overflow_menu(ui, c, out));

    let tree = Kind::Tree;
    if bar_button(
        ui,
        ICON_ACCOUNT_TREE,
        "Session tree (Ctrl+Shift+B)",
        session_actions::availability(tree, c).is_enabled(),
        reason(tree, c),
        false,
    )
    .clicked()
    {
        out.push(BarAction::OpenTree);
    }

    let fork = Kind::Fork;
    if bar_button(
        ui,
        ICON_FORK_RIGHT,
        "Fork into a new tab",
        session_actions::availability(fork, c).is_enabled(),
        reason(fork, c),
        false,
    )
    .clicked()
    {
        out.push(BarAction::Run(Action::Fork));
    }

    let compact = bar_button(
        ui,
        ICON_COMPRESS,
        "Compact context…",
        session_actions::availability(Kind::Compact, c).is_enabled(),
        reason(Kind::Compact, c),
        false,
    );
    if session_actions::availability(Kind::Compact, c).is_enabled() {
        theme::dropup_menu(&compact, 210.0, |ui| {
            if menu_item(ui, ICON_COMPRESS, "Compact", None, true, false).clicked() {
                out.push(BarAction::Run(Action::Compact(CompactMode::Default)));
            }
            if menu_item(ui, ICON_COMPRESS, "Compact (soft)", None, true, false).clicked() {
                out.push(BarAction::Run(Action::Compact(CompactMode::Soft)));
            }
            if menu_item(ui, ICON_COMPRESS, "Snapcompact", None, true, false).clicked() {
                out.push(BarAction::Run(Action::Compact(CompactMode::Snapcompact)));
            }
            ui.separator();
            if menu_item(ui, ICON_EDIT, "Compact with focus…", None, true, false).clicked() {
                out.push(BarAction::AskCompactFocus);
            }
        });
    }

    let handoff = bar_button(
        ui,
        ICON_HANDSHAKE,
        "Hand off to a fresh context…",
        session_actions::availability(Kind::Handoff, c).is_enabled(),
        reason(Kind::Handoff, c),
        false,
    );
    if handoff.clicked() && session_actions::availability(Kind::Handoff, c).is_enabled() {
        out.push(BarAction::AskHandoff);
    }

    if bar_button(
        ui,
        ICON_MAP,
        "Plan mode (toggle)",
        session_actions::availability(Kind::PlanMode, c).is_enabled(),
        reason(Kind::PlanMode, c),
        false,
    )
    .clicked()
    {
        out.push(BarAction::Run(Action::PlanMode));
    }

    if session_actions::visible(Kind::Retry, c)
        && bar_button(
            ui,
            ICON_REPLAY,
            "Retry the failed turn",
            session_actions::availability(Kind::Retry, c).is_enabled(),
            reason(Kind::Retry, c),
            true,
        )
        .clicked()
    {
        out.push(BarAction::Run(Action::Retry));
    }
    if session_actions::visible(Kind::Stop, c)
        && bar_button(
            ui,
            ICON_STOP,
            "Stop the current turn (Esc)",
            true,
            None,
            true,
        )
        .clicked()
    {
        out.push(BarAction::Run(Action::Stop));
    }
}

fn reason(kind: Kind, c: &ActionCtx) -> Option<&'static str> {
    match session_actions::availability(kind, c) {
        Availability::Enabled => None,
        Availability::Disabled(r) => Some(r),
    }
}

fn overflow_menu(ui: &mut Ui, c: &ActionCtx, out: &mut Vec<BarAction>) {
    let mut item = |ui: &mut Ui, icon, text: &str, kind: Kind, act: BarAction| {
        let a = session_actions::availability(kind, c);
        let r = menu_item(ui, icon, text, None, a.is_enabled(), false);
        let r = match a {
            Availability::Disabled(why) => r.on_disabled_hover_text(why),
            Availability::Enabled => r,
        };
        if r.clicked() {
            out.push(act);
        }
    };
    item(
        ui,
        ICON_ACCOUNT_TREE,
        "Session tree",
        Kind::Tree,
        BarAction::OpenTree,
    );
    item(
        ui,
        ICON_TERMINAL,
        "Open omp's Agent Hub (TUI)",
        Kind::AgentHub,
        BarAction::Run(Action::AgentHub),
    );
    item(
        ui,
        ICON_DONUT_LARGE,
        "Context usage",
        Kind::Context,
        BarAction::Run(Action::Context),
    );
    ui.separator();
    item(ui, ICON_EDIT, "Rename…", Kind::Rename, BarAction::AskRename);
    item(
        ui,
        ICON_AUTO_AWESOME,
        "Auto-name",
        Kind::AutoName,
        BarAction::Run(Action::AutoName),
    );
    ui.separator();
    item(
        ui,
        ICON_CONTENT_COPY,
        "Copy last response",
        Kind::CopyLast,
        BarAction::Run(Action::CopyLast),
    );
    item(
        ui,
        ICON_DOWNLOAD,
        "Export as HTML",
        Kind::Export,
        BarAction::Run(Action::ExportHtml),
    );
}

/// Compact icon button; disabled ones dim and explain why in the tooltip. `emphasis` tints it.
fn bar_button(
    ui: &mut Ui,
    icon: egui_material_icons::MaterialIcon,
    tooltip: &str,
    enabled: bool,
    disabled_reason: Option<&str>,
    emphasis: bool,
) -> Response {
    let pal = Palette::of_ui(ui);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, CHIP_H), sense);
    if resp.clicked() {
        resp.surrender_focus();
    }
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() && enabled, 0.1);
    let base = if emphasis { pal.warn } else { pal.weak };
    let (fg_rest, fg_hot) = if enabled {
        (base, if emphasis { pal.warn } else { pal.text_strong })
    } else {
        (pal.weak.gamma_multiply(0.4), pal.weak.gamma_multiply(0.4))
    };
    let p = ui.painter();
    p.rect_filled(rect, 10, pal.raised.gamma_multiply(0.9 * hov));
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        21.6,
        theme::mix(fg_rest, fg_hot, hov),
    );
    let tip = match (enabled, disabled_reason) {
        (false, Some(why)) => format!("{tooltip}\n{why}"),
        _ => tooltip.to_owned(),
    };
    resp.on_hover_text(tip).on_hover_cursor(if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::NotAllowed
    })
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::status::{PrInfo, SessionStats};

    fn catalog() -> ModelsCache {
        let mut c = ModelsCache::default();
        c.finish(Ok(vec![ModelInfo {
            selector: "anthropic/claude-opus-5-5".to_owned(),
            provider: "anthropic".to_owned(),
            id: "claude-opus-5-5".to_owned(),
            name: "Claude Opus 5.5".to_owned(),
            context_window: Some(1_000_000),
            thinking: vec!["low".into(), "high".into()],
            cost_input: Some(5.0),
            cost_output: Some(25.0),
        }]));
        c
    }

    fn status() -> SessionStatus {
        SessionStatus {
            cwd: "/tmp/proj".into(),
            stats: SessionStats {
                model: Some("anthropic/claude-opus-5-5".into()),
                thinking: Some("high".into()),
                thinking_configured: Some("auto".into()),
                input_tokens: 50_000,
                output_tokens: 200,
                cache_read: 1_000,
                cache_write: 12_952,
                cost: 7.0,
                context_tokens: Some(173_000),
                ..SessionStats::default()
            },
            git: Some(GitInfo {
                branch: "feat/x".into(),
                ahead: 2,
                behind: 0,
                staged: 1,
                unstaged: 3,
                untracked: 2,
                pr: Some(PrInfo {
                    number: 12,
                    url: "https://example.com/pr/12".into(),
                }),
            }),
            ..SessionStatus::default()
        }
    }

    #[test]
    fn view_formats_every_segment() {
        let mut v = BarView::default();
        let snap = Arc::new(status());
        assert!(v.update(Some(&snap), Path::new("/x"), &catalog()));
        assert_eq!(v.model_name, "Claude Opus 5.5");
        assert_eq!(v.current_level(), Some("auto"));
        assert_eq!(v.ctx_label, "17.3%/1M");
        assert_eq!(v.ctx_level, Some(ContextLevel::Warn));
        assert_eq!(v.tokens_in, "50K");
        assert_eq!(v.tokens_out, "200");
        assert_eq!(v.cost, "$7.00");
        assert!(v.show_cost && v.show_tokens);
        let g = v.git.as_ref().expect("git");
        assert_eq!(g.sync, "↑2");
        assert_eq!(
            (g.unstaged.as_str(), g.staged.as_str(), g.untracked.as_str()),
            ("*3", "+1", "?2")
        );
        assert_eq!(
            g.pr,
            Some(("#12".to_owned(), "https://example.com/pr/12".to_owned()))
        );
        assert!(g.dirty);
        assert!(v.path_full.ends_with("proj"));
    }

    #[test]
    fn view_rebuilds_only_on_new_snapshot_or_catalog() {
        let mut v = BarView::default();
        let snap = Arc::new(status());
        let cat = catalog();
        assert!(v.update(Some(&snap), Path::new("/x"), &cat));
        assert!(!v.update(Some(&snap), Path::new("/x"), &cat));
        let newer = Arc::new(status());
        assert!(v.update(Some(&newer), Path::new("/x"), &cat));
        let mut cat2 = catalog();
        cat2.finish(Ok(Vec::new()));
        assert!(v.update(Some(&newer), Path::new("/x"), &cat2));
        assert_eq!(v.model_name, "claude-opus-5-5");
        assert_eq!(v.ctx_label, "173K/?");
    }

    #[test]
    fn fresh_session_shows_omp_defaults_until_a_model_is_recorded() {
        let defaults = Defaults {
            model: Some("anthropic/claude-opus-5-5".into()),
            level: Some("high".into()),
        };
        let mut v = BarView::default();
        v.set_defaults(&defaults);
        assert!(v.update(None, Path::new("/work/dir"), &catalog()));
        assert!(v.model_is_default);
        assert_eq!(v.model_name, "Claude Opus 5.5");
        assert_eq!(v.current_level(), Some("high"));
        assert!(v.model_tip.contains("Default model"));
        // The session's own model replaces the default.
        let snap = Arc::new(status());
        assert!(v.update(Some(&snap), Path::new("/x"), &catalog()));
        assert!(!v.model_is_default);
        assert_eq!(v.current_level(), Some("auto"));
        // Defaults loaded late rebuild an already built view.
        let mut late = BarView::default();
        assert!(late.update(None, Path::new("/w"), &catalog()));
        assert!(late.model_name.is_empty());
        late.set_defaults(&defaults);
        assert!(late.update(None, Path::new("/w"), &catalog()));
        assert_eq!(late.model_name, "Claude Opus 5.5");
    }

    #[test]
    fn fresh_session_hides_tokens_and_cost() {
        let mut v = BarView::default();
        assert!(v.update(None, Path::new("/work/dir"), &catalog()));
        assert!(!v.show_tokens && !v.show_cost);
        assert!(v.git.is_none());
        assert_eq!(v.path_full, "/work/dir");
        assert_eq!(v.ctx_label, "?");
    }

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn segments_drop_monotonically_as_the_bar_narrows() {
        // ctx is narrow, path wide: greedy skipping would show ctx but not path at some width,
        // and path but not ctx at another.
        let widths = [60.0, 90.0, 300.0, 70.0, 80.0];
        let all = [true; 5];
        let mut prev = [true; 5];
        let mut w = 800.0;
        while w >= 0.0 {
            let now = fit_segments(&widths, &all, w, 6.0);
            for i in 0..5 {
                assert!(prev[i] || !now[i], "segment {i} came back at {w}px");
            }
            // Never a lower-priority segment without every higher-priority one.
            for i in 1..5 {
                assert!(
                    !now[i] || now[i - 1],
                    "segment {i} shown without {} at {w}",
                    i - 1
                );
            }
            prev = now;
            w -= 5.0;
        }
    }

    #[test]
    fn model_chip_is_wide_but_capped_and_segments_still_fit_monotonically() {
        assert!((model_chip_width(100.0, 1400.0).0 - 276.9).abs() < 0.1);
        assert!((model_chip_width(300.0, 1400.0).0 - 390.0).abs() < 0.1);
        assert_eq!(model_chip_width(300.0, 400.0).0, 208.0);
        let widths = [60.0, 90.0, 300.0, 70.0, 80.0];
        let all = [true; 5];
        let mut prev = [true; 5];
        let mut bar = 1400.0;
        while bar >= 0.0 {
            let (chip, _) = model_chip_width(widths[2], bar);
            let now = fit_segments(&widths, &all, bar - chip - 6.0, 6.0);
            for i in 0..5 {
                assert!(prev[i] || !now[i], "segment {i} came back at {bar}px");
            }
            prev = now;
            bar -= 5.0;
        }
    }

    #[test]
    fn absent_segments_do_not_block_the_rest() {
        let widths = [60.0, 90.0, 100.0, 70.0, 80.0];
        let present = [true, false, true, false, true];
        assert_eq!(
            fit_segments(&widths, &present, 400.0, 6.0),
            [true, false, true, false, true]
        );
        // Path no longer fits: tokens (lower priority) goes too.
        assert_eq!(
            fit_segments(&widths, &present, 150.0, 6.0),
            [true, false, false, false, false]
        );
    }
}
