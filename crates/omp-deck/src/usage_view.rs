//! The Usage view (omp's `/usage`, natively): a modal with an Overview (stats strip, quota cards,
//! activity heatmap) and a Details tab (per-account limit tables, top models), plus the compact
//! quota cards the status-bar chip shows on hover.
//!
//! Everything painted here is pre-formatted by [`crate::usage_state`]; this module only lays out.

use std::sync::Arc;

use deck_core::usage::{Heatmap, LimitStatus};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Galley, Id, Key, Layout, Margin, Modifiers, Order,
    Painter, Pos2, Rect, RichText, Sense, Sides, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::icons::{
    ICON_ACTIVITY, ICON_CIRCLE_ALERT, ICON_GAUGE, ICON_KEY_ROUND, ICON_LIST_TREE, ICON_REFRESH_CW,
    ICON_SPARKLES, ICON_TERMINAL, ICON_X,
};
use crate::theme::{self, Palette};
use crate::usage_fmt::{long_date, money, requests};
use crate::usage_state::{
    ActivityView, CARD_ROWS, CardView, DetailBlock, Load, ModelRow, NoDataWarning, QuotaRow,
    Summary, Tile,
};
use crate::widgets::{Spin, empty_state, icon_text, paint_icon};

const GRID_GAP: f32 = 2.5;
const DAY_LETTERS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];

/// Which page of the view is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Overview,
    Details,
}

#[derive(Default)]
pub struct ViewState {
    pub page: Page,
}

/// What the view needs from the app.
pub struct ViewInput<'a> {
    pub load: Load,
    pub summary: Option<&'a Arc<Summary>>,
    pub activity: Option<&'a Arc<ActivityView>>,
    pub error: Option<&'a str>,
    pub activity_error: Option<&'a str>,
    pub activity_loading: bool,
    pub fetching: bool,
    pub age_secs: Option<u64>,
}

pub enum UsageAction {
    Close,
    Refresh,
    /// Run omp's official installer (omp was not found).
    InstallOmp,
}

/// Colour of a limit status; `Ok` is the accent (teal in Neutral).
pub fn status_color(pal: &Palette, s: LimitStatus) -> Color32 {
    match s {
        LimitStatus::Ok => pal.accent,
        LimitStatus::Warning => pal.warn,
        LimitStatus::Exhausted => pal.danger,
        LimitStatus::Unknown => pal.weak,
    }
}

fn track(pal: &Palette) -> Color32 {
    theme::mix(pal.surface, pal.weak, 0.28)
}

// ------------------------------------------------------------------ window

pub fn show(ctx: &egui::Context, st: &mut ViewState, input: &ViewInput<'_>) -> Vec<UsageAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();

    egui::Area::new(Id::unique("usage-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() {
                out.push(UsageAction::Close);
            }
        });

    if !egui::Popup::is_any_open(ctx)
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        out.push(UsageAction::Close);
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) && !input.fetching {
        out.push(UsageAction::Refresh);
    }

    let size = vec2(
        (screen.width() * 0.62)
            .clamp(660.0, 1000.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.86)
            .clamp(440.0, 900.0)
            .min(screen.height() - 24.0),
    );
    let style = ctx.global_style();
    let frame = Frame::NONE
        .fill(pal.bar)
        .stroke(theme::popup_stroke(pal))
        .corner_radius(theme::popup_radius())
        .inner_margin(Margin::symmetric(20, 16))
        .shadow(style.visuals.window_shadow);
    egui::Window::new("usage-window")
        .id(Id::unique("usage-window"))
        .title_bar(false)
        .frame(frame)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(600.0, 380.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| body(ui, st, input, &mut out));
    out
}

fn body(ui: &mut Ui, st: &mut ViewState, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    header(ui, input, out);
    ui.add_space(10.0);
    tabs(ui, st);
    ui.add_space(12.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            // Keep the scrollbar off the content.
            ui.set_width(ui.available_width() - 6.0);
            match st.page {
                Page::Overview => overview(ui, input, out),
                Page::Details => details(ui, input, out),
            }
        });
}

fn header(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            ui.painter().rect_filled(
                rect,
                theme::rad(8, 10),
                if theme::neutral() {
                    pal.surface
                } else {
                    pal.accent.gamma_multiply(0.18)
                },
            );
            paint_icon(
                ui.painter(),
                rect.center(),
                Align2::CENTER_CENTER,
                ICON_GAUGE,
                19.0,
                pal.glyph,
            );
            ui.label(theme::strong("Usage").size(19.0).color(pal.text_strong));
            let when = crate::usage_fmt::header_when(
                input.fetching,
                input.age_secs,
                input.summary.is_some_and(|s| s.warning.is_some()),
            );
            if !when.is_empty() {
                ui.label(RichText::new(when).size(13.0).color(pal.weak));
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if ui
                .add_sized(
                    [30.0, 30.0],
                    egui::Button::new(icon_text(ICON_X, 20.0, pal.weak)).frame(false),
                )
                .on_hover_text("Close (Esc)")
                .clicked()
            {
                out.push(UsageAction::Close);
            }
            if input.fetching {
                let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                Spin::new(17.0, pal.accent).paint_at(ui, rect.center());
            } else if ui
                .add_sized(
                    [30.0, 30.0],
                    egui::Button::new(icon_text(ICON_REFRESH_CW, 18.0, pal.weak)).frame(false),
                )
                .on_hover_text("Refresh now (R)")
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                out.push(UsageAction::Refresh);
            }
        },
    );
}

fn tabs(ui: &mut Ui, st: &mut ViewState) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (page, label) in [(Page::Overview, "Overview"), (Page::Details, "Details")] {
            if tab_chip(ui, label, st.page == page).clicked() {
                st.page = page;
            }
        }
    });
}

fn tab_chip(ui: &mut Ui, label: &str, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::font(13.0), pal.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 28.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let r = theme::rad(8, 14);
    if on {
        p.rect_filled(
            rect,
            r,
            if theme::neutral() {
                pal.raised
            } else {
                pal.accent.gamma_multiply(0.22)
            },
        );
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.7)),
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
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ overview

fn overview(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    stats_strip(ui, input);
    ui.add_space(6.0);
    section(ui, "Quotas");
    quotas(ui, input, out);
    ui.add_space(6.0);
    section(ui, "Activity");
    activity(ui, input);
}

fn section(ui: &mut Ui, title: &str) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::section_caption(title, pal.weak));
}

fn pick<'a>(t: Option<&'a Tile>, none: &'a str) -> (&'a str, &'a str) {
    t.map_or(("—", none), |t| (t.value.as_str(), t.sub.as_str()))
}

fn card_frame(pal: &Palette) -> Frame {
    Frame::NONE
        .fill(if theme::neutral() {
            pal.panel
        } else {
            pal.surface.gamma_multiply(0.75)
        })
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(theme::rad(8, 14))
        .inner_margin(Margin::symmetric(14, 11))
}

fn stats_strip(ui: &mut Ui, input: &ViewInput<'_>) {
    let pal = Palette::of_ui(ui);
    ui.columns(4, |cols| {
        // Tightest quota.
        let tight = input.summary.and_then(|s| s.chip.as_ref());
        tile(
            &mut cols[0],
            "Tightest quota",
            tight.map_or("—", |c| c.text.as_str()),
            tight.map_or(pal.weak, |c| status_color(pal, c.status)),
            tight.map_or("No quota data", |c| c.tight.as_str()),
            None,
        );
        let act = input.activity;
        let none = if input.activity_loading {
            "Reading activity…"
        } else {
            "No activity recorded"
        };
        let (v, s) = pick(act.filter(|a| !a.empty).map(|a| &a.past_year), none);
        tile(&mut cols[1], "Past year", v, pal.text_strong, s, None);
        let (v, s) = pick(act.filter(|a| !a.empty).map(|a| &a.last30), none);
        tile(
            &mut cols[2],
            "Last 30 days",
            v,
            pal.text_strong,
            s,
            act.filter(|a| !a.empty)
                .map(|a| a.heatmap.last30.daily.as_slice()),
        );
        let (v, s) = pick(
            act.filter(|a| !a.empty).and_then(|a| a.busiest.as_ref()),
            none,
        );
        tile(&mut cols[3], "Busiest day", v, pal.text_strong, s, None);
    });
}

fn tile(
    ui: &mut Ui,
    caption: &str,
    value: &str,
    value_color: Color32,
    sub: &str,
    spark: Option<&[f64]>,
) {
    let pal = Palette::of_ui(ui);
    card_frame(pal)
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(theme::section_caption(caption, pal.weak));
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(theme::strong(value).size(21.0).color(value_color))
                        .selectable(false),
                );
                if let Some(d) = spark {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        sparkline(ui, d, vec2(ui.available_width().min(64.0), 22.0));
                    });
                }
            });
            ui.add(
                egui::Label::new(RichText::new(sub).small().color(pal.weak))
                    .truncate()
                    .selectable(false),
            );
        });
}

fn sparkline(ui: &mut Ui, data: &[f64], size: egui::Vec2) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let max = data.iter().copied().fold(0.0_f64, f64::max);
    if data.len() < 2 || max <= 0.0 {
        let y = rect.bottom() - 1.0;
        ui.painter().hline(
            rect.x_range(),
            y,
            Stroke::new(1.0, pal.stroke.gamma_multiply(1.4)),
        );
        return;
    }
    let n = (data.len() - 1) as f32;
    let pts: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            pos2(
                rect.left() + rect.width() * i as f32 / n,
                rect.bottom() - 2.0 - (rect.height() - 4.0) * (*v / max) as f32,
            )
        })
        .collect();
    let line = if theme::neutral() {
        pal.glyph
    } else {
        pal.accent
    };
    ui.painter()
        .add(egui::Shape::line(pts, Stroke::new(1.5, line)));
}

// ------------------------------------------------------------------ quotas

fn quotas(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    match input.load {
        Load::OmpMissing => {
            empty_state(
                ui,
                ICON_TERMINAL,
                pal.warn,
                "omp was not found",
                "OmpDeck reads quotas with `omp usage`. Install omp, or put it on PATH and restart OmpDeck.",
            );
            ui.add_space(12.0);
            if ui
                .vertical_centered(crate::widgets::install_omp_button)
                .inner
            {
                out.push(UsageAction::InstallOmp);
            }
            return;
        }
        Load::Loading => {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                Spin::new(22.0, pal.accent).paint_at(ui, rect.center());
                ui.label(RichText::new("Reading quotas from omp…").color(pal.weak));
            });
            ui.add_space(16.0);
            return;
        }
        Load::Failed => {
            error_state(ui, input.error.unwrap_or("unknown error"), out);
            return;
        }
        Load::Ready => {}
    }
    let Some(summary) = input.summary else {
        return;
    };
    if let Some(e) = input.error {
        error_strip(ui, e, out);
    }
    if let Some(w) = &summary.warning {
        warning_state(ui, w, out);
        return;
    }
    if summary.cards.is_empty() {
        empty_state(
            ui,
            ICON_KEY_ROUND,
            pal.accent,
            "No accounts with usage data",
            "Log in to a provider with `omp /login` — quotas for Anthropic, Codex, Copilot and others appear here.",
        );
        for u in &summary.orphans {
            unavailable_row(ui, u);
        }
        return;
    }
    let wide =
        ui.available_width() >= 640.0 && summary.cards.iter().filter(|c| !c.idle).count() > 1;
    let active = summary.cards.iter().filter(|c| !c.idle);
    if wide {
        ui.columns(2, |cols| {
            let mut heights = [0.0_f32; 2];
            for c in active {
                let col = usize::from(heights[1] < heights[0]);
                heights[col] += c.weight;
                provider_card(&mut cols[col], c, false);
            }
        });
    } else {
        for c in active {
            provider_card(ui, c, false);
        }
    }
    if !summary.untouched.is_empty() {
        ui.label(RichText::new(&summary.untouched).small().color(pal.weak));
    }
    for u in &summary.orphans {
        unavailable_row(ui, u);
    }
}
fn warning_state(ui: &mut Ui, w: &NoDataWarning, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    empty_state(
        ui,
        ICON_CIRCLE_ALERT,
        pal.warn,
        &w.title,
        "Every provider fetch failed or is backing off, so there are no quotas to show.",
    );
    for a in &w.accounts {
        unavailable_row(ui, a);
    }
    ui.add_space(4.0);
    ui.vertical_centered(|ui| {
        if ui.button("Retry").clicked() {
            out.push(UsageAction::Refresh);
        }
    });
}

fn error_state(ui: &mut Ui, msg: &str, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    empty_state(
        ui,
        ICON_CIRCLE_ALERT,
        pal.danger,
        "Could not read usage",
        msg,
    );
    ui.vertical_centered(|ui| {
        if ui.button("Retry").clicked() {
            out.push(UsageAction::Refresh);
        }
    });
}

fn error_strip(ui: &mut Ui, msg: &str, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(theme::mix(pal.bar, pal.warn, 0.10))
        .stroke(theme::status_stroke(pal, pal.warn, 0.35))
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(icon_text(ICON_CIRCLE_ALERT, 15.0, pal.warn));
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("Refresh failed — showing the last data. {msg}"))
                            .small()
                            .color(pal.text),
                    )
                    .truncate(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Retry").clicked() {
                        out.push(UsageAction::Refresh);
                    }
                });
            });
        });
}

fn unavailable_row(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(icon_text(ICON_CIRCLE_ALERT, 13.0, pal.weak));
        ui.add(egui::Label::new(RichText::new(text).small().color(pal.weak)).truncate());
    });
}

/// One provider: status dot + name, then up to four window rows and the extras.
pub fn provider_card(ui: &mut Ui, c: &CardView, compact: bool) {
    let pal = Palette::of_ui(ui);
    let frame = if compact {
        Frame::NONE
    } else {
        card_frame(pal)
    };
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = if compact { 3.0 } else { 5.0 };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            let (rect, _) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 4.0, status_color(pal, c.status));
            ui.label(theme::strong(&c.name).size(14.5).color(pal.text_strong));
            if let Some(a) = &c.accounts {
                ui.label(RichText::new(a).small().color(pal.weak));
            }
        });
        if c.unlimited {
            ui.label(RichText::new("Unlimited").small().color(pal.weak));
        }
        for w in c.rows.iter().take(CARD_ROWS) {
            window_row(ui, w, compact);
        }
        let more = if compact {
            &c.more_compact
        } else {
            &c.more_full
        };
        if let Some(text) = more {
            ui.label(RichText::new(text).small().color(pal.weak));
        }
        if let Some(r) = &c.reset_credits {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_text(ICON_SPARKLES, 13.0, pal.accent));
                ui.add(egui::Label::new(RichText::new(r).small().color(pal.text)).truncate());
            });
        }
        for a in &c.unavailable {
            unavailable_row(ui, a);
        }
        if !compact {
            for a in &c.daybreak {
                ui.label(RichText::new(a).small().color(pal.weak));
            }
            for n in &c.notes {
                ui.add(egui::Label::new(RichText::new(n).small().color(pal.weak)).truncate());
            }
        }
    });
}

/// `label  [5h]  ......  63% left` over a remaining-bar with `resets in …` on its right.
fn window_row(ui: &mut Ui, w: &QuotaRow, compact: bool) {
    let pal = Palette::of_ui(ui);
    let color = status_color(pal, w.status);
    ui.spacing_mut().item_spacing.y = 3.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add(
            egui::Label::new(RichText::new(&w.label).color(pal.text))
                .truncate()
                .selectable(false),
        );
        if let Some(tag) = &w.tag {
            ui.label(RichText::new(tag).small().color(pal.weak));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(v) = &w.value {
                if w.measured {
                    ui.label(theme::strong(v).color(color));
                } else {
                    ui.label(RichText::new(v).color(pal.weak));
                }
            }
        });
    });
    if let Some(left) = w.remaining {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let reset_w = if compact { 92.0 } else { 104.0 };
            let bar_w = if w.reset.is_some() {
                (ui.available_width() - reset_w - 10.0).max(40.0)
            } else {
                ui.available_width()
            };
            remaining_bar(ui, bar_w, left, color);
            if let Some(r) = &w.reset {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(r).small().color(pal.weak));
                });
            }
        });
    }
}

fn remaining_bar(ui: &mut Ui, width: f32, left: f32, color: Color32) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3, track(pal));
    if left > 0.0 {
        let w = (rect.width() * left).max(4.0).min(rect.width());
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(w, rect.height())),
            3,
            color,
        );
    }
}

// ------------------------------------------------------------------ chip hover

/// The compact tooltip of the status-bar chip.
pub fn hover_ui(ui: &mut Ui, summary: &Summary) {
    let pal = Palette::of_ui(ui);
    ui.set_width(312.0);
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        ui.label(icon_text(ICON_GAUGE, 16.0, pal.glyph));
        ui.label(theme::strong("Usage").color(pal.text_strong));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("Ctrl+Shift+U").small().color(pal.weak));
        });
    });
    let mut first = true;
    for c in summary.cards.iter().filter(|c| !c.idle) {
        if !first {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(rect, 0, pal.stroke);
        }
        first = false;
        provider_card(ui, c, true);
    }
    if first {
        ui.label(RichText::new("Nothing used yet").small().color(pal.weak));
    }
    if !summary.untouched.is_empty() {
        ui.label(RichText::new(&summary.untouched).small().color(pal.weak));
    }
    ui.label(
        RichText::new("Click for the full Usage view")
            .small()
            .color(pal.weak),
    );
}

// ------------------------------------------------------------------ heatmap

fn level_color(pal: &Palette, level: u8) -> Color32 {
    if level == 0 {
        return theme::mix(
            pal.surface,
            pal.weak,
            if pal.bg.r() < 128 { 0.20 } else { 0.28 },
        );
    }
    let t = f32::from(level.min(4)) / 4.0;
    let top = if theme::neutral() {
        pal.accent
    } else {
        theme::mix(pal.accent_a, pal.accent_b, t)
    };
    theme::mix(theme::mix(pal.surface, top, 0.22), top, t)
}

fn activity(ui: &mut Ui, input: &ViewInput<'_>) {
    let pal = Palette::of_ui(ui);
    if let Some(e) = input.activity_error {
        ui.label(
            RichText::new(format!("Could not read omp's activity data: {e}"))
                .small()
                .color(pal.danger),
        );
        return;
    }
    let Some(a) = input.activity else {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            Spin::new(16.0, pal.accent).paint_at(ui, rect.center());
            ui.label(RichText::new("Reading activity…").small().color(pal.weak));
        });
        return;
    };
    if a.empty {
        card_frame(pal).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(icon_text(ICON_ACTIVITY, 16.0, pal.weak));
            ui.label(
                RichText::new(
                    "No activity yet — omp records your requests in stats.db as you work.",
                )
                .color(pal.weak),
            );
        });
        return;
    }
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        heatmap(ui, &a.heatmap);
    });
}

fn heatmap(ui: &mut Ui, hm: &Heatmap) {
    let pal = Palette::of_ui(ui);
    let label_w = 14.0;
    let avail = ui.available_width();
    let cs = ((avail - label_w - 6.0 - GRID_GAP * (hm.weeks as f32 - 1.0)) / hm.weeks as f32)
        .clamp(7.0, 16.0);
    let step = cs + GRID_GAP;
    let top = 15.0;
    let size = vec2(label_w + 6.0 + step * hm.weeks as f32, top + step * 7.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let origin = pos2(rect.left() + label_w + 6.0, rect.top() + top);
    let hover = resp.hover_pos().and_then(|p| {
        let (dx, dy) = (p.x - origin.x, p.y - origin.y);
        if dx < 0.0 || dy < 0.0 {
            return None;
        }
        let (week, row) = ((dx / step) as usize, (dy / step) as usize);
        (week < hm.weeks && row < 7 && hm.cells[row][week].is_some()).then_some((row, week))
    });
    let p = ui.painter();
    let small = theme::font(10.5);
    for (row, letter) in DAY_LETTERS.iter().enumerate() {
        p.text(
            pos2(rect.left(), origin.y + step * row as f32 + cs / 2.0),
            Align2::LEFT_CENTER,
            *letter,
            small.clone(),
            pal.weak.gamma_multiply(0.8),
        );
    }
    let mut last_label_x = f32::NEG_INFINITY;
    for week in 0..hm.weeks {
        let x = origin.x + step * week as f32;
        if let Some(Some(name)) = hm.month_labels.get(week)
            && x - last_label_x >= 26.0
        {
            p.text(
                pos2(x, rect.top() + 1.0),
                Align2::LEFT_TOP,
                *name,
                small.clone(),
                pal.weak,
            );
            last_label_x = x;
        }
        for row in 0..7 {
            let Some(level) = hm.cells[row][week] else {
                continue;
            };
            let r = Rect::from_min_size(pos2(x, origin.y + step * row as f32), vec2(cs, cs));
            p.rect_filled(r, (cs * 0.22).round() as u8, level_color(pal, level));
        }
    }
    if let Some((row, week)) = hover {
        let r = Rect::from_min_size(
            pos2(origin.x + step * week as f32, origin.y + step * row as f32),
            vec2(cs, cs),
        );
        p.rect_stroke(
            r.expand(1.0),
            (cs * 0.3).round() as u8,
            Stroke::new(1.5, pal.text_strong),
            StrokeKind::Outside,
        );
        resp.on_hover_ui_at_pointer(|ui| cell_tip(ui, hm, row, week));
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.label(
            RichText::new(crate::usage_fmt::streak_text(
                hm.active_days,
                hm.longest_streak,
            ))
            .small()
            .color(pal.weak),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("More").small().color(pal.weak));
            for level in (0..=4).rev() {
                let (r, _) = ui.allocate_exact_size(vec2(11.0, 11.0), Sense::hover());
                ui.painter().rect_filled(r, 2, level_color(pal, level));
            }
            ui.label(RichText::new("Less").small().color(pal.weak));
        });
    });
}

fn cell_tip(ui: &mut Ui, hm: &Heatmap, row: usize, week: usize) {
    let pal = Palette::of_ui(ui);
    let t = hm.totals[row][week];
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.label(theme::strong(long_date(hm.date_at(row, week))).color(pal.text_strong));
    if t.requests == 0 && t.cost <= 0.0 {
        ui.label(RichText::new("No activity").color(pal.weak));
        return;
    }
    ui.label(RichText::new(money(t.cost)).color(pal.text));
    ui.label(RichText::new(requests(t.requests)).color(pal.text));
    ui.label(
        RichText::new(format!(
            "{} tokens",
            crate::statusbar::group_digits(t.tokens)
        ))
        .color(pal.weak),
    );
}

// ------------------------------------------------------------------ details

fn details(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    match input.load {
        Load::OmpMissing | Load::Loading | Load::Failed => {
            quotas(ui, input, out);
            return;
        }
        Load::Ready => {}
    }
    let Some(summary) = input.summary else {
        return;
    };
    if let Some(e) = input.error {
        error_strip(ui, e, out);
    }
    if let Some(w) = &summary.warning {
        warning_state(ui, w, out);
    } else if summary.details.is_empty() {
        empty_state(
            ui,
            ICON_LIST_TREE,
            pal.accent,
            "No limits reported",
            "No logged-in provider reported any usage limits.",
        );
    }
    for b in &summary.details {
        detail_block(ui, b);
    }
    ui.add_space(6.0);
    section(ui, "Top models · last 30 days");
    match input.activity {
        Some(a) if !a.models.is_empty() => models_table(ui, &a.models),
        Some(_) => {
            ui.label(
                RichText::new("No model usage in the last 30 days.")
                    .small()
                    .color(pal.weak),
            );
        }
        None => {
            ui.label(
                RichText::new(if input.activity_loading {
                    "Reading activity…"
                } else {
                    "Activity data is unavailable."
                })
                .small()
                .color(pal.weak),
            );
        }
    }
}

const ROW_H: f32 = 26.0;

fn elided(p: &Painter, text: &str, size: f32, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat {
            font_id: theme::font(size),
            color,
            ..Default::default()
        },
    );
    job.wrap = TextWrapping::truncate_at_width(width.max(8.0));
    p.layout_job(job)
}

fn cell(p: &Painter, text: &str, color: Color32, x: f32, w: f32, mid_y: f32, right: bool) {
    let g = elided(p, text, 13.0, color, w);
    let px = if right { x + w - g.size().x } else { x };
    p.galley(pos2(px, mid_y - g.size().y / 2.0), g, color);
}

fn detail_block(ui: &mut Ui, b: &DetailBlock) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.label(theme::strong(&b.name).size(14.5).color(pal.text_strong));
            if !b.meta.is_empty() {
                ui.label(RichText::new(&b.meta).small().color(pal.weak));
            }
        });
        ui.add_space(4.0);
        let w = ui.available_width();
        let cols = [w * 0.34, w * 0.27, w * 0.19, w * 0.20];
        let (head, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        let p = ui.painter();
        let mut x = head.left();
        for (i, title) in ["LIMIT", "ACCOUNT", "LEFT", "RESETS"].iter().enumerate() {
            p.text(
                pos2(x, head.center().y),
                Align2::LEFT_CENTER,
                *title,
                theme::font(10.5),
                pal.weak,
            );
            x += cols[i];
        }
        p.hline(head.x_range(), head.bottom(), Stroke::new(1.0, pal.stroke));
        for r in &b.rows {
            let (row, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
            let p = ui.painter();
            let y = row.center().y;
            let mut x = row.left();
            cell(p, &r.limit, pal.text, x, cols[0] - 10.0, y, false);
            x += cols[0];
            cell(p, &r.account, pal.weak, x, cols[1] - 10.0, y, false);
            x += cols[1];
            let color = status_color(pal, r.status);
            if let Some(f) = r.left_frac {
                let bar = Rect::from_min_size(pos2(x, y - 3.0), vec2(36.0, 6.0));
                p.rect_filled(bar, 3, track(pal));
                if f > 0.0 {
                    p.rect_filled(
                        Rect::from_min_size(bar.min, vec2((bar.width() * f).max(3.0), 6.0)),
                        3,
                        color,
                    );
                }
                cell(p, &r.left, color, x + 44.0, cols[2] - 54.0, y, false);
            } else {
                cell(p, &r.left, pal.text, x, cols[2] - 10.0, y, false);
            }
            x += cols[2];
            cell(p, &r.resets, pal.weak, x, cols[3], y, false);
        }
    });
}

fn models_table(ui: &mut Ui, rows: &[ModelRow]) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let cols = [w * 0.36, w * 0.18, w * 0.14, w * 0.16, w * 0.16];
        let (head, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        let p = ui.painter();
        let mut x = head.left();
        for (i, title) in ["MODEL", "PROVIDER", "COST", "REQUESTS", "TOKENS"]
            .iter()
            .enumerate()
        {
            let right = i >= 2;
            p.text(
                pos2(if right { x + cols[i] - 10.0 } else { x }, head.center().y),
                if right {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::LEFT_CENTER
                },
                *title,
                theme::font(10.5),
                pal.weak,
            );
            x += cols[i];
        }
        p.hline(head.x_range(), head.bottom(), Stroke::new(1.0, pal.stroke));
        for r in rows.iter().take(12) {
            let (row, _) = ui.allocate_exact_size(vec2(w, ROW_H + 3.0), Sense::hover());
            let p = ui.painter();
            let y = row.center().y - 1.5;
            let mut x = row.left();
            cell(p, &r.model, pal.text, x, cols[0] - 10.0, y, false);
            x += cols[0];
            cell(p, &r.provider, pal.weak, x, cols[1] - 10.0, y, false);
            x += cols[1];
            cell(p, &r.cost, pal.text_strong, x, cols[2] - 10.0, y, true);
            x += cols[2];
            cell(p, &r.requests, pal.text, x, cols[3] - 10.0, y, true);
            x += cols[3];
            cell(p, &r.tokens, pal.weak, x, cols[4] - 10.0, y, true);
            // Share of the busiest model, drawn under the model name.
            let track = Rect::from_min_size(
                pos2(row.left(), row.bottom() - 5.0),
                vec2(cols[0] - 10.0, 3.0),
            );
            p.rect_filled(track, 1.5, pal.surface);
            let bar = Rect::from_min_size(
                track.min,
                vec2(track.width() * r.share.clamp(0.0, 1.0), 3.0),
            );
            p.rect_filled(
                bar,
                1,
                if theme::neutral() {
                    pal.glyph.gamma_multiply(0.45)
                } else {
                    pal.accent.gamma_multiply(0.55)
                },
            );
        }
    });
}
