//! Reusable chrome widgets: icon buttons, window buttons, toggle, segmented control, cards,
//! setting rows, empty states and menu items. Everything is themed through [`Palette`].

use crate::icons::{ICON_DOWNLOAD, Icon};
use egui::{
    Align2, Button, Color32, CornerRadius, CursorIcon, FontId, Frame, Margin, Response, RichText,
    Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::theme::{self, Palette};

pub fn icon_text(icon: Icon, size: f32, color: Color32) -> RichText {
    icon.rich_text().size(size).color(color)
}

pub fn paint_icon(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: Align2,
    icon: Icon,
    size: f32,
    color: Color32,
) {
    painter.text(
        pos,
        align,
        icon.codepoint,
        FontId::new(size, icon.font_family()),
        color,
    );
}

/// Square borderless icon button with animated hover; `on` keeps it highlighted (open menu, page).
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str, on: bool) -> Response {
    let pal = Palette::of_ui(ui);
    let side = theme::dens(28.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() || on, 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let (radius, size) = if theme::flat() { (6, 16.0) } else { (9, 19.0) };
        p.rect_filled(
            rect,
            radius,
            theme::hover_fill(pal).gamma_multiply(0.85 * hov),
        );
        paint_icon(
            p,
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            size,
            theme::mix(pal.weak, pal.text_strong, hov),
        );
    }
    resp.on_hover_text(tooltip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// Title-bar window button (minimize / maximize / close); close turns red on hover.
pub fn win_button(ui: &mut Ui, icon: Icon, tooltip: &str, danger: bool) -> Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let fill = if danger {
            pal.danger
        } else if pal.bg.r() < 128 {
            Color32::from_white_alpha(30)
        } else {
            Color32::from_black_alpha(24)
        };
        p.rect_filled(
            rect,
            if theme::flat() { 6 } else { 9 },
            fill.gamma_multiply(hov),
        );
        let fg = if danger && hov > 0.5 {
            Color32::WHITE
        } else {
            theme::mix(pal.weak, pal.text_strong, hov)
        };
        let size = if theme::flat() { 15.0 } else { 17.0 };
        paint_icon(p, rect.center(), Align2::CENTER_CENTER, icon, size, fg);
    }
    resp.on_hover_text(tooltip)
}

/// Size of the switch drawn by [`toggle`].
pub fn toggle_size(_ui: &Ui) -> egui::Vec2 {
    if theme::flat() {
        vec2(34.0, 20.0)
    } else {
        vec2(42.0, 24.0)
    }
}

/// Animated switch. Returns the response with `changed()` set on toggle.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let pal = Palette::of_ui(ui);
    let neutral = theme::flat();
    let size = toggle_size(ui);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let ctx = ui.ctx();
    let t = ctx.animate_bool_with_time(resp.id, *on, 0.15);
    let hov = ctx.animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
    let off = theme::mix(pal.raised, pal.stroke, hov);
    let p = ui.painter();
    if neutral {
        let track = theme::mix(theme::mix(pal.raised, pal.weak, 0.25 * hov), pal.accent, t);
        p.rect_filled(rect, 10, track);
        let knob_off = if pal.bg.r() < 128 { pal.text } else { pal.bg };
        let knob_on = if pal.bg.r() < 128 {
            pal.bg
        } else {
            Color32::WHITE
        };
        let r = rect.height() / 2.0 - 3.0;
        let cx = egui::lerp((rect.left() + r + 3.0)..=(rect.right() - r - 3.0), t);
        p.circle_filled(
            pos2(cx, rect.center().y),
            r,
            theme::mix(knob_off, knob_on, t),
        );
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *on, "")
        });
        return resp.on_hover_cursor(CursorIcon::PointingHand);
    }
    p.rect(
        rect,
        12,
        off,
        Stroke::new(1.0, theme::mix(pal.stroke, pal.accent_a, t)),
        StrokeKind::Inside,
    );
    if t > 0.01 {
        theme::gradient_rounded(
            p,
            rect.shrink(1.0),
            11.0,
            pal.accent_a.gamma_multiply(t),
            pal.accent_b.gamma_multiply(t),
        );
    }
    let cx = egui::lerp((rect.left() + 12.0)..=(rect.right() - 12.0), t);
    p.circle_filled(
        pos2(cx, rect.center().y + 1.0),
        8.5,
        Color32::from_black_alpha(50),
    );
    p.circle_filled(
        pos2(cx, rect.center().y),
        8.5,
        theme::mix(pal.weak, Color32::WHITE, t),
    );
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *on, ""));
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Segmented control; returns true when the selection changed.
pub fn segmented(ui: &mut Ui, selected: &mut usize, options: &[(Icon, &str)]) -> bool {
    let pal = Palette::of_ui(ui);
    let neutral = theme::flat();
    let font = crate::theme::font(if neutral { 13.0 } else { 13.5 });
    let icon_w = 16.0;
    let pad = if neutral { 10.0 } else { 12.0 };
    let h = if neutral {
        theme::dens(24.0, 28.0)
    } else {
        30.0
    };
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), pal.text)
        })
        .collect();
    let widths: Vec<f32> = galleys
        .iter()
        .map(|g| pad + icon_w + 6.0 + g.size().x + pad)
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let id = ui.next_auto_id().with("segmented");
    let (rect, _) = ui.allocate_exact_size(vec2(total, h + 6.0), Sense::hover());
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(if neutral { 8 } else { 11 }),
        if neutral { pal.surface } else { pal.bg },
        Stroke::new(1.0, pal.stroke),
        StrokeKind::Inside,
    );
    // Segment rects.
    let mut x = rect.left() + 3.0;
    let segs: Vec<egui::Rect> = widths
        .iter()
        .map(|w| {
            let r = egui::Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(*w, h));
            x += w;
            r
        })
        .collect();
    let mut changed = false;
    let t = ui
        .ctx()
        .animate_value_with_time(id, *selected as f32, 0.16)
        .clamp(0.0, (segs.len().max(1) - 1) as f32);
    let (lo, hi) = (t.floor() as usize, t.ceil() as usize);
    let f = t - t.floor();
    if let (Some(a), Some(b)) = (segs.get(lo), segs.get(hi)) {
        let pill = egui::Rect::from_min_max(a.min.lerp(b.min, f), a.max.lerp(b.max, f));
        if neutral {
            let fill = if pal.bg.r() < 128 { pal.raised } else { pal.bg };
            p.rect(
                pill,
                6,
                fill,
                Stroke::new(1.0, theme::mix(pal.stroke, pal.weak, 0.3)),
                StrokeKind::Inside,
            );
        } else {
            p.rect_filled(pill, 8, pal.accent.gamma_multiply(0.30));
            p.rect_stroke(
                pill,
                8,
                Stroke::new(1.0, pal.accent.gamma_multiply(0.8)),
                StrokeKind::Inside,
            );
        }
    }
    for (i, seg) in segs.iter().enumerate() {
        let resp = ui.interact(*seg, id.with(i), Sense::click());
        if resp.clicked() && *selected != i {
            *selected = i;
            changed = true;
        }
        let on = *selected == i;
        let hov = resp.hovered() && !on;
        let color = if on {
            pal.text_strong
        } else if hov {
            pal.text
        } else {
            pal.weak
        };
        let p = ui.painter();
        paint_icon(
            p,
            pos2(seg.left() + pad + icon_w / 2.0, seg.center().y),
            Align2::CENTER_CENTER,
            options[i].0,
            if neutral { 15.0 } else { 17.0 },
            color,
        );
        p.galley(
            pos2(
                seg.left() + pad + icon_w + 6.0,
                seg.center().y - galleys[i].size().y / 2.0,
            ),
            galleys[i].clone(),
            color,
        );
        resp.on_hover_cursor(CursorIcon::PointingHand);
    }
    changed
}

/// Titled card (rounded surface with caption).
pub fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    let pal = Palette::of_ui(ui);
    let neutral = theme::flat();
    Frame::NONE
        .fill(if neutral {
            pal.panel
        } else {
            pal.surface.gamma_multiply(0.75)
        })
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(if neutral { 8 } else { 14 })
        .inner_margin(if neutral {
            Margin::symmetric(16, 12)
        } else {
            Margin::symmetric(20, 16)
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::section_caption(title, pal.weak));
            ui.add_space(6.0);
            add(ui);
        });
    ui.add_space(if neutral { 12.0 } else { 14.0 });
}

/// Title + description on the left, control on the right.
pub fn setting_row(ui: &mut Ui, title: &str, sub: &str, right: impl FnOnce(&mut Ui)) {
    let pal = Palette::of_ui(ui);
    egui::Sides::new()
        .shrink_left()
        .height(theme::dens(40.0, 44.0))
        .show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new(title).color(pal.text_strong));
                    ui.label(RichText::new(sub).small().color(pal.weak));
                });
            },
            right,
        );
}

/// Centered icon-in-circle + title + description.
pub fn empty_state(ui: &mut Ui, icon: Icon, color: Color32, title: &str, sub: &str) {
    let pal = Palette::of_ui(ui);
    ui.add_space(28.0);
    ui.vertical_centered(|ui| {
        if theme::flat() {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            let p = ui.painter();
            p.rect(
                rect,
                10,
                pal.surface,
                Stroke::new(1.0, pal.stroke),
                StrokeKind::Inside,
            );
            let tint = if color == pal.accent {
                pal.glyph
            } else {
                color
            };
            paint_icon(p, rect.center(), Align2::CENTER_CENTER, icon, 20.0, tint);
        } else {
            let (rect, _) = ui.allocate_exact_size(vec2(52.0, 52.0), Sense::hover());
            let p = ui.painter();
            p.circle_filled(rect.center(), 26.0, color.gamma_multiply(0.14));
            p.circle_stroke(
                rect.center(),
                26.0,
                Stroke::new(1.0, color.gamma_multiply(0.35)),
            );
            paint_icon(p, rect.center(), Align2::CENTER_CENTER, icon, 26.0, color);
        }
        ui.add_space(8.0);
        ui.label(theme::strong(title).size(14.5).color(pal.text_strong));
        ui.add(
            egui::Label::new(RichText::new(sub).small().color(pal.weak))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
}

/// "Install omp" button shown wherever omp was not found.
pub fn install_omp_button(ui: &mut Ui) -> bool {
    let pal = Palette::of_ui(ui);
    ui.add(
        Button::new((
            icon_text(ICON_DOWNLOAD, 16.0, pal.glyph),
            RichText::new("Install omp"),
        ))
        .min_size(vec2(0.0, 30.0)),
    )
    .on_hover_text(format!(
        "Run omp's official installer ({}) in a new tab; it installs to {}, no admin rights needed",
        deck_core::omp::INSTALL_URL,
        crate::update::OMP_INSTALL_DIR
    ))
    .clicked()
}

/// Menu entry with icon, optional shortcut hint and danger styling.
pub fn menu_item(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    shortcut: Option<&str>,
    enabled: bool,
    danger: bool,
) -> Response {
    let pal = Palette::of_ui(ui);
    let (ic, fg) = if danger {
        (pal.danger, pal.danger)
    } else {
        (pal.glyph, pal.text)
    };
    let mut btn = Button::new((
        icon_text(icon, if theme::flat() { 15.5 } else { 17.0 }, ic),
        RichText::new(label).color(fg),
    ));
    if let Some(s) = shortcut {
        btn = btn.shortcut_text(RichText::new(s).small().color(pal.weak));
    }
    ui.add_enabled(enabled, btn)
}

/// How often [`Spin`] advances (steps per second); egui's own spinner repaints every frame, which
/// pegs the CPU while a turn runs.
const SPIN_FPS: f64 = 10.0;

/// Rotating-arc busy indicator that repaints at [`SPIN_FPS`] instead of continuously.
pub struct Spin {
    size: f32,
    color: Color32,
}

impl Spin {
    pub fn new(size: f32, color: Color32) -> Spin {
        Spin { size, color }
    }

    /// Draw centered on `center` without allocating layout space (overlays on custom-painted
    /// rows, where `ui.put` would pull the parent's cursor back into the row).
    pub fn paint_at(self, ui: &Ui, center: egui::Pos2) {
        let rect = egui::Rect::from_center_size(center, vec2(self.size, self.size));
        if !ui.is_rect_visible(rect) {
            return;
        }
        const STEPS: f64 = 12.0;
        let step = (ui.input(|i| i.time) * SPIN_FPS).floor() % STEPS;
        let start = (step / STEPS) as f32 * std::f32::consts::TAU;
        let radius = self.size / 2.0 - 2.0;
        let points: Vec<egui::Pos2> = (0..=18)
            .map(|i| {
                let a = start + (i as f32 / 18.0) * std::f32::consts::TAU * 0.75;
                rect.center() + radius * vec2(a.cos(), a.sin())
            })
            .collect();
        ui.painter()
            .add(egui::Shape::line(points, Stroke::new(2.0, self.color)));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis((1000.0 / SPIN_FPS) as u64));
    }
}

impl egui::Widget for Spin {
    fn ui(self, ui: &mut Ui) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(self.size, self.size), Sense::hover());
        self.paint_at(ui, rect.center());
        resp
    }
}

/// Role of a [`DraftButton`]: neutral, the accent action, or a destructive one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DraftKind {
    #[default]
    Secondary,
    Primary,
    Danger,
}

/// "Drafted" push button: uppercase tracked label, hairline border, an outer rule 2 px outside
/// it and a hard base shadow that the body sinks into when pressed. Primary / Danger add a
/// tinted gradient fill, an inner top highlight, a deep tone base and a soft glow. Focusable:
/// Enter / Space click it like a pointer click.
#[must_use = "call `.show(ui)` or `ui.add(..)`"]
pub struct DraftButton<'a> {
    label: &'a str,
    kind: DraftKind,
    icon: Option<Icon>,
    enabled: bool,
    min_width: f32,
}

impl<'a> DraftButton<'a> {
    pub fn new(label: &'a str) -> Self {
        DraftButton {
            label,
            kind: DraftKind::Secondary,
            icon: None,
            enabled: true,
            min_width: 0.0,
        }
    }

    pub fn kind(mut self, kind: DraftKind) -> Self {
        self.kind = kind;
        self
    }

    /// Leading glyph.
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        ui.add(self)
    }
}

/// Horizontal padding of a [`DraftButton`].
const DRAFT_PAD_X: f32 = 16.0;
/// Gap between a [`DraftButton`]'s icon and label.
const DRAFT_ICON_GAP: f32 = 8.0;
const DRAFT_ICON: f32 = 14.0;

/// Height of a [`DraftButton`] at the active density.
pub fn draft_height() -> f32 {
    theme::dens(34.0, 36.0)
}

/// Room a [`DraftButton`] paints below its rect (hard base shadow and outer rule).
pub const DRAFT_BELOW: f32 = 4.0;

impl egui::Widget for DraftButton<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let pal = Palette::of_ui(ui);
        let dark = ui.visuals().dark_mode;
        let enabled = self.enabled && ui.is_enabled();
        let job = egui::text::LayoutJob::single_section(
            self.label.to_uppercase(),
            egui::TextFormat {
                font_id: theme::label_font(12.0),
                extra_letter_spacing: 1.0,
                color: Color32::PLACEHOLDER,
                ..Default::default()
            },
        );
        let galley = ui.painter().layout_job(job);
        let icon_w = if self.icon.is_some() {
            DRAFT_ICON + DRAFT_ICON_GAP
        } else {
            0.0
        };
        let size = vec2(
            (DRAFT_PAD_X * 2.0 + icon_w + galley.size().x).max(self.min_width),
            draft_height(),
        );
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, self.label));
        if ui.is_rect_visible(rect.expand(4.0)) {
            let look = draft_look(pal, dark, self.kind, enabled);
            let hov = ui.ctx().animate_bool_with_time(
                resp.id.with("hov"),
                enabled && resp.hovered(),
                0.1,
            );
            let pressed = enabled && resp.is_pointer_button_down_on();
            paint_draft(
                ui,
                rect,
                &look,
                DraftState {
                    hov,
                    pressed,
                    focus: resp.has_focus(),
                    enabled,
                },
                self.icon,
                galley,
            );
        }
        if enabled {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }
}

/// Resting colours of a [`DraftButton`] (hover blends toward `border_hov` / `rule_hov`).
#[derive(Clone, Copy, Debug)]
struct DraftLook {
    fill: Color32,
    /// Gradient fill over `fill` (`top`, `bottom`), `None` for Secondary.
    tint: Option<(Color32, Color32)>,
    text: Color32,
    text_hov: Color32,
    border: Color32,
    border_hov: Color32,
    rule: Color32,
    rule_hov: Color32,
    /// Hard base shadow; transparent when disabled.
    base: Color32,
    /// Inner top highlight and glow (tinted kinds only).
    highlight: Color32,
    glow: Color32,
}

#[derive(Clone, Copy, Debug)]
struct DraftState {
    hov: f32,
    pressed: bool,
    focus: bool,
    enabled: bool,
}

fn draft_look(pal: &Palette, dark: bool, kind: DraftKind, enabled: bool) -> DraftLook {
    let fill = theme::mix(pal.window, pal.raised, if dark { 0.35 } else { 0.2 });
    let tone = match kind {
        DraftKind::Secondary => None,
        DraftKind::Primary => Some(pal.accent),
        DraftKind::Danger => Some(pal.danger),
    };
    let rest_border = theme::mix(pal.stroke, pal.hover_line, 0.15);
    if !enabled {
        return DraftLook {
            fill,
            tint: None,
            text: pal.weak,
            text_hov: pal.weak,
            border: tone.map_or(rest_border, |t| theme::mix(pal.stroke, t, 0.35)),
            border_hov: tone.map_or(rest_border, |t| theme::mix(pal.stroke, t, 0.35)),
            rule: pal.stroke.gamma_multiply(0.35),
            rule_hov: pal.stroke.gamma_multiply(0.35),
            base: Color32::TRANSPARENT,
            highlight: Color32::TRANSPARENT,
            glow: Color32::TRANSPARENT,
        };
    }
    match tone {
        None => DraftLook {
            fill,
            tint: None,
            text: pal.text,
            text_hov: pal.text_strong,
            border: rest_border,
            border_hov: pal.hover_line,
            rule: pal.stroke.gamma_multiply(0.7),
            rule_hov: pal.hover_line.gamma_multiply(0.5),
            base: Color32::from_black_alpha(if dark { 140 } else { 46 }),
            highlight: Color32::TRANSPARENT,
            glow: Color32::TRANSPARENT,
        },
        Some(t) => DraftLook {
            fill,
            tint: Some((theme::mix(fill, t, 0.28), theme::mix(fill, t, 0.12))),
            text: pal.text_strong,
            text_hov: pal.text_strong,
            border: t,
            border_hov: theme::mix(t, pal.text_strong, 0.25),
            rule: t.gamma_multiply(0.45),
            rule_hov: t.gamma_multiply(0.7),
            base: theme::mix(t, Color32::BLACK, 0.75).gamma_multiply(0.9),
            highlight: theme::mix(t, Color32::WHITE, 0.4).gamma_multiply(0.35),
            glow: t.gamma_multiply(0.18),
        },
    }
}

fn paint_draft(
    ui: &Ui,
    rect: egui::Rect,
    look: &DraftLook,
    st: DraftState,
    icon: Option<Icon>,
    galley: std::sync::Arc<egui::Galley>,
) {
    let pal = Palette::of_ui(ui);
    let p = ui.painter();
    let r = theme::look().ui_r.min(4);
    let border_w = if theme::style() == deck_core::config::UiStyle::NeoBrutal {
        2.0
    } else {
        1.0
    };
    let (sink, base_dy) = if st.pressed { (2.0, 1.0) } else { (0.0, 3.0) };
    let body = rect.translate(vec2(0.0, sink));

    if look.base.a() > 0 {
        p.rect_filled(body.translate(vec2(0.0, base_dy)), r, look.base);
    }
    if look.glow.a() > 0 {
        crate::deco::glow(p, body, r, look.glow.gamma_multiply(1.0 + 0.5 * st.hov), 16);
    }
    p.rect_filled(body, r, look.fill);
    if let Some((top, bottom)) = look.tint {
        let lift = 0.06 * st.hov;
        crate::deco::fill_vertical(
            p,
            &crate::deco::rounded_rect(body, f32::from(r)),
            theme::mix(top, look.border, lift),
            theme::mix(bottom, look.border, lift),
        );
    }
    if look.highlight.a() > 0 {
        let y = body.top() + border_w + 0.5;
        p.line_segment(
            [
                egui::pos2(body.left() + f32::from(r) + 1.0, y),
                egui::pos2(body.right() - f32::from(r) - 1.0, y),
            ],
            Stroke::new(1.0, look.highlight),
        );
    }
    p.rect_stroke(
        body,
        r,
        Stroke::new(border_w, theme::mix(look.border, look.border_hov, st.hov)),
        StrokeKind::Inside,
    );
    let rule = if st.focus && st.enabled {
        pal.accent.gamma_multiply(0.9)
    } else {
        theme::mix(look.rule, look.rule_hov, st.hov)
    };
    p.rect_stroke(
        body.expand(2.0 + border_w),
        r + 2 + border_w as u8,
        Stroke::new(1.0, rule),
        StrokeKind::Inside,
    );

    let text = theme::mix(look.text, look.text_hov, st.hov);
    let icon_w = if icon.is_some() {
        DRAFT_ICON + DRAFT_ICON_GAP
    } else {
        0.0
    };
    let content_w = icon_w + galley.size().x;
    let left = body.center().x - content_w / 2.0;
    if let Some(i) = icon {
        paint_icon(
            p,
            egui::pos2(left + DRAFT_ICON / 2.0, body.center().y),
            Align2::CENTER_CENTER,
            i,
            DRAFT_ICON,
            text,
        );
    }
    let pos = egui::pos2(left + icon_w, body.center().y - galley.size().y / 2.0);
    p.galley(pos, galley, text);
}

/// How long a [`stepper`] value must stay unchanged before it is reported, so a burst of clicks
/// (and the relayout an app-wide zoom causes) applies once instead of once per click.
pub const STEPPER_SETTLE: f64 = 0.4;

#[derive(Clone, Default)]
struct StepperState {
    /// Value shown but not reported yet, and when it last changed.
    pending: Option<(f32, f64)>,
    /// Field text while the user types.
    text: Option<String>,
}

#[derive(Clone, Copy)]
enum StepperPart {
    Minus,
    Field,
    Unit,
    Plus,
}

/// `−` [value] unit `+` for a whole-number setting in `lo..=hi`. The buttons move to the
/// previous / next multiple of `step`; the field takes any whole number (Enter or focus loss
/// applies, Escape reverts; out-of-range values are clamped). Returns the new value once it has
/// settled for [`STEPPER_SETTLE`] (typed values at once), never per click.
pub fn stepper(
    ui: &mut Ui,
    id_salt: &str,
    value: f32,
    (lo, hi): (f32, f32),
    step: f32,
    unit: &str,
) -> Option<f32> {
    let pal = Palette::of_ui(ui);
    let id = ui.scope_id().with(("stepper", id_salt));
    let now = ui.input(|i| i.time);
    let mut st: StepperState = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let shown = st.pending.map_or(value, |(v, _)| v);
    // (value, apply at once)
    let mut set: Option<(f32, bool)> = None;
    let mut parts = [
        StepperPart::Minus,
        StepperPart::Field,
        StepperPart::Unit,
        StepperPart::Plus,
    ];
    // Setting rows lay their control out right to left: add in reverse visual order.
    if ui.layout().prefer_right_to_left() {
        parts.reverse();
    }
    ui.spacing_mut().item_spacing.x = 4.0;
    for part in parts {
        match part {
            StepperPart::Minus | StepperPart::Plus => {
                let up = matches!(part, StepperPart::Plus);
                let (icon, tip, enabled) = if up {
                    (crate::icons::ICON_PLUS, "Larger", shown < hi - 1e-3)
                } else {
                    (crate::icons::ICON_MINUS, "Smaller", shown > lo + 1e-3)
                };
                let resp = ui
                    .add_enabled(
                        enabled,
                        Button::new(icon_text(icon, 15.0, pal.text)).min_size(vec2(28.0, 28.0)),
                    )
                    .on_hover_text(tip);
                if resp.clicked() {
                    set = Some((step_value(shown, step, up).clamp(lo, hi), false));
                }
            }
            StepperPart::Field => {
                let mut text = st.text.clone().unwrap_or_else(|| format!("{shown:.0}"));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .desired_width(40.0)
                        .align(Align2::CENTER_CENTER)
                        .font(theme::font(13.5)),
                );
                if resp.lost_focus() {
                    if !ui.input(|i| i.key_pressed(egui::Key::Escape))
                        && let Some(v) = parse_whole(&text)
                    {
                        set = Some((v.clamp(lo, hi), true));
                    }
                    st.text = None;
                } else if resp.has_focus() {
                    st.text = Some(text);
                }
            }
            StepperPart::Unit => {
                // Fixed width, so steppers with different units line up in a column.
                ui.add_sized(
                    vec2(18.0, 28.0),
                    egui::Label::new(RichText::new(unit).color(pal.weak)).selectable(false),
                );
            }
        }
    }
    if let Some((v, at_once)) = set {
        let at = if at_once { now - STEPPER_SETTLE } else { now };
        st.pending = Some((v, at));
    }
    let mut out = None;
    if let Some((v, at)) = st.pending {
        let left = STEPPER_SETTLE - (now - at);
        if left <= 0.0 {
            st.pending = None;
            out = ((v - value).abs() > 1e-3).then_some(v);
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(left));
        }
    }
    ui.data_mut(|d| d.insert_temp(id, st));
    out
}

/// The next (`up`) or previous multiple of `step` from `v`; a value already on the grid moves a
/// whole step.
fn step_value(v: f32, step: f32, up: bool) -> f32 {
    let k = v / step;
    if up {
        ((k + 1e-3).floor() + 1.0) * step
    } else {
        ((k - 1e-3).ceil() - 1.0) * step
    }
}

/// A typed number with an optional unit (`130`, `130 %`, `14pt`), rounded to a whole number.
fn parse_whole(text: &str) -> Option<f32> {
    text.trim()
        .trim_end_matches(|c: char| c.is_alphabetic() || c == '%' || c.is_whitespace())
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .map(f32::round)
}

#[cfg(test)]
mod tests {
    use super::{parse_whole, step_value};

    #[test]
    fn stepper_buttons_snap_to_the_step_grid() {
        assert_eq!(step_value(130.0, 5.0, true), 135.0);
        assert_eq!(step_value(130.0, 5.0, false), 125.0);
        // Off-grid values (typed 133 %) go to the neighbouring multiple, not 138 / 128.
        assert_eq!(step_value(133.0, 5.0, true), 135.0);
        assert_eq!(step_value(133.0, 5.0, false), 130.0);
        assert_eq!(step_value(14.0, 1.0, true), 15.0);
        assert_eq!(step_value(14.0, 1.0, false), 13.0);
    }

    #[test]
    fn typed_values_accept_units_and_reject_garbage() {
        assert_eq!(parse_whole(" 130 % "), Some(130.0));
        assert_eq!(parse_whole("14pt"), Some(14.0));
        assert_eq!(parse_whole("12.6"), Some(13.0));
        assert_eq!(parse_whole(""), None);
        assert_eq!(parse_whole("abc"), None);
        assert_eq!(parse_whole("inf"), None);
    }
}
