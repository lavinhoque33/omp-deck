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
        let (radius, size) = if theme::neutral() {
            (6, 16.0)
        } else {
            (9, 19.0)
        };
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
            if theme::neutral() { 6 } else { 9 },
            fill.gamma_multiply(hov),
        );
        let fg = if danger && hov > 0.5 {
            Color32::WHITE
        } else {
            theme::mix(pal.weak, pal.text_strong, hov)
        };
        let size = if theme::neutral() { 15.0 } else { 17.0 };
        paint_icon(p, rect.center(), Align2::CENTER_CENTER, icon, size, fg);
    }
    resp.on_hover_text(tooltip)
}

/// Animated switch. Returns the response with `changed()` set on toggle.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let pal = Palette::of_ui(ui);
    let neutral = theme::neutral();
    let size = if neutral {
        vec2(34.0, 20.0)
    } else {
        vec2(42.0, 24.0)
    };
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
    let neutral = theme::neutral();
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
    let neutral = theme::neutral();
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
        if theme::neutral() {
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
        icon_text(icon, if theme::neutral() { 15.5 } else { 17.0 }, ic),
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
