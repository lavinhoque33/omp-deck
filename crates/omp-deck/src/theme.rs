//! "Midnight neon" chrome theme: palette, egui `Style`/`Visuals` for dark and light, and the
//! small painting helpers (gradients, glow) shared by every chrome widget.
//!
//! The terminal grid keeps its own colors (`deck_term::TermTheme`); this only styles the chrome.

use egui::epaint::Mesh;
use egui::style::{HandleShape, ScrollStyle};
use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Painter, Pos2, Rect, RichText,
    Shadow, Stroke, Style, TextStyle, Theme, ThemePreference, Ui, Vec2, vec2,
};

/// Colour tokens for one theme variant.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Terminal surround / deepest layer.
    pub bg: Color32,
    /// Sidebar and page background.
    pub panel: Color32,
    /// Title/tab bar.
    pub bar: Color32,
    /// Cards, inputs.
    pub surface: Color32,
    /// Hover fill, popups' highlighted row.
    pub raised: Color32,
    pub stroke: Color32,
    pub text: Color32,
    pub text_strong: Color32,
    pub weak: Color32,
    /// Gradient start (violet).
    pub accent_a: Color32,
    /// Gradient end (cyan).
    pub accent_b: Color32,
    /// Solid accent for strokes, selection, focus.
    pub accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
}

const DARK: Palette = Palette {
    bg: Color32::from_rgb(8, 11, 22),
    panel: Color32::from_rgb(12, 16, 31),
    bar: Color32::from_rgb(14, 19, 37),
    surface: Color32::from_rgb(20, 27, 49),
    raised: Color32::from_rgb(30, 39, 70),
    stroke: Color32::from_rgb(36, 46, 78),
    text: Color32::from_rgb(212, 220, 240),
    text_strong: Color32::from_rgb(244, 247, 255),
    weak: Color32::from_rgb(125, 137, 172),
    accent_a: Color32::from_rgb(139, 92, 246),
    accent_b: Color32::from_rgb(34, 211, 238),
    accent: Color32::from_rgb(124, 108, 255),
    ok: Color32::from_rgb(52, 211, 153),
    warn: Color32::from_rgb(251, 191, 36),
    danger: Color32::from_rgb(248, 113, 113),
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(244, 246, 252),
    panel: Color32::from_rgb(236, 239, 248),
    bar: Color32::from_rgb(228, 232, 245),
    surface: Color32::from_rgb(252, 253, 255),
    raised: Color32::from_rgb(222, 227, 244),
    stroke: Color32::from_rgb(207, 213, 233),
    text: Color32::from_rgb(38, 46, 78),
    text_strong: Color32::from_rgb(14, 19, 46),
    weak: Color32::from_rgb(106, 116, 148),
    accent_a: Color32::from_rgb(124, 77, 235),
    accent_b: Color32::from_rgb(8, 163, 200),
    accent: Color32::from_rgb(110, 84, 240),
    ok: Color32::from_rgb(16, 160, 110),
    warn: Color32::from_rgb(200, 130, 0),
    danger: Color32::from_rgb(214, 54, 54),
};

impl Palette {
    pub fn of(dark: bool) -> &'static Palette {
        if dark { &DARK } else { &LIGHT }
    }

    pub fn of_ui(ui: &Ui) -> &'static Palette {
        Self::of(ui.visuals().dark_mode)
    }

    pub fn of_ctx(ctx: &Context) -> &'static Palette {
        Self::of(ctx.global_style().visuals.dark_mode)
    }
}

/// Installs both theme variants; call once after fonts are installed.
pub fn install(ctx: &Context) {
    ctx.style_mut_of(Theme::Dark, |s| build(s, &DARK, true));
    ctx.style_mut_of(Theme::Light, |s| build(s, &LIGHT, false));
}

/// Switches the active variant.
pub fn select(ctx: &Context, dark: bool) {
    ctx.set_theme(if dark {
        ThemePreference::Dark
    } else {
        ThemePreference::Light
    });
}

fn build(style: &mut Style, p: &Palette, dark: bool) {
    let prop = |size| FontId::new(size, FontFamily::Proportional);
    style.text_styles = [
        (TextStyle::Small, prop(12.0)),
        (TextStyle::Body, prop(14.0)),
        (TextStyle::Button, prop(14.0)),
        (TextStyle::Heading, prop(22.0)),
        (
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
        (TextStyle::Name("Section".into()), prop(11.5)),
        (TextStyle::Name("Title".into()), prop(15.0)),
    ]
    .into();
    style.animation_time = 0.12;
    style.interaction.tooltip_delay = 0.4;

    let sp = &mut style.spacing;
    sp.item_spacing = vec2(8.0, 6.0);
    sp.button_padding = vec2(12.0, 6.0);
    sp.interact_size = vec2(36.0, 30.0);
    sp.menu_margin = Margin::same(6);
    sp.window_margin = Margin::same(16);
    sp.menu_width = 200.0;
    sp.tooltip_width = 420.0;
    sp.slider_rail_height = 6.0;
    sp.slider_width = 180.0;
    sp.scroll = ScrollStyle::floating();
    sp.scroll.bar_width = 6.0;
    sp.scroll.floating_width = 4.0;

    let v = &mut style.visuals;
    v.dark_mode = dark;
    v.weak_text_color = Some(p.weak);
    v.panel_fill = p.panel;
    v.window_fill = if dark {
        Color32::from_rgb(18, 24, 44)
    } else {
        Color32::WHITE
    };
    v.extreme_bg_color = mix(p.bg, p.raised, 0.55);
    v.text_edit_bg_color = Some(p.surface);
    v.faint_bg_color = p.surface;
    v.code_bg_color = p.surface;
    v.hyperlink_color = p.accent_b;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.35 } else { 0.28 });
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent_b);
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, p.stroke);
    let shadow = |dy: i8, blur: u8, a: u8| Shadow {
        offset: [0, dy],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(if dark { a } else { a / 3 }),
    };
    v.window_shadow = shadow(12, 32, 150);
    v.popup_shadow = shadow(6, 22, 130);

    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.surface;
    w.inactive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;

    w.hovered.bg_fill = p.raised;
    w.hovered.weak_bg_fill = p.raised;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text_strong);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.accent.gamma_multiply(0.5);
    w.active.weak_bg_fill = p.accent.gamma_multiply(0.5);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text_strong);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.raised;
    w.open.weak_bg_fill = p.raised;
    w.open.bg_stroke = Stroke::new(1.0, p.stroke);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;
}

/// egui re-applies `menu_style` (button padding 2×0, transparent inactive fill, no strokes) to every
/// menu and submenu; this keeps that look but restores roomy item padding.
pub fn menu_style(style: &mut Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding = vec2(10.0, 6.0);
    style.spacing.item_spacing.y = 2.0;
    style.spacing.interact_size.y = 28.0;
}

/// Context menu for `resp` (right click) with the chrome menu style.
pub fn context_menu<R>(
    resp: &egui::Response,
    min_width: f32,
    f: impl FnOnce(&mut Ui) -> R,
) -> Option<egui::InnerResponse<R>> {
    egui::Popup::context_menu(resp)
        .style(menu_style)
        .width(min_width)
        .show(f)
}

/// Dropdown menu below a toggle button, right-aligned to it.
pub fn dropdown_menu<R>(
    resp: &egui::Response,
    min_width: f32,
    f: impl FnOnce(&mut Ui) -> R,
) -> Option<egui::InnerResponse<R>> {
    egui::Popup::menu(resp)
        .align(egui::RectAlign::BOTTOM_END)
        .style(menu_style)
        .gap(6.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(min_width)
        .show(f)
}

/// Dropdown menu below a toggle button, left-aligned to it.
pub fn dropdown_start_menu<R>(
    resp: &egui::Response,
    min_width: f32,
    f: impl FnOnce(&mut Ui) -> R,
) -> Option<egui::InnerResponse<R>> {
    egui::Popup::menu(resp)
        .align(egui::RectAlign::BOTTOM_START)
        .style(menu_style)
        .gap(6.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(min_width)
        .show(f)
}

/// Menu opening above a button, right-aligned to it (for the bottom status bar).
pub fn dropup_menu<R>(
    resp: &egui::Response,
    min_width: f32,
    f: impl FnOnce(&mut Ui) -> R,
) -> Option<egui::InnerResponse<R>> {
    egui::Popup::menu(resp)
        .align(egui::RectAlign::TOP_END)
        .style(menu_style)
        .gap(8.0)
        .width(min_width)
        .show(f)
}

/// Semi-bold text via the UI font's `wght` axis (no-op for fixed-weight fallbacks).
pub fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text).variation("wght", 640.0)
}

/// Small uppercase caption used above groups.
pub fn section_caption(text: &str, color: Color32) -> RichText {
    RichText::new(text.to_uppercase())
        .text_style(TextStyle::Name("Section".into()))
        .color(color)
        .variation("wght", 700.0)
}

/// Axis-aligned two-colour gradient.
pub fn gradient_rect(painter: &Painter, rect: Rect, a: Color32, b: Color32, horizontal: bool) {
    let (tl, tr, bl, br) = if horizontal {
        (a, b, a, b)
    } else {
        (a, a, b, b)
    };
    let mut m = Mesh::default();
    m.colored_vertex(rect.left_top(), tl);
    m.colored_vertex(rect.right_top(), tr);
    m.colored_vertex(rect.left_bottom(), bl);
    m.colored_vertex(rect.right_bottom(), br);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(m));
}

/// Rounded rectangle filled with a horizontal gradient (triangle fan, colour by x).
pub fn gradient_rounded(painter: &Painter, rect: Rect, radius: f32, a: Color32, b: Color32) {
    let radius = radius.min(rect.height() / 2.0).min(rect.width() / 2.0);
    let col = |x: f32| a.lerp_to_gamma(b, ((x - rect.min.x) / rect.width()).clamp(0.0, 1.0));
    let mut m = Mesh::default();
    m.colored_vertex(rect.center(), col(rect.center().x));
    let corners = [
        (rect.right_top() + vec2(-radius, radius), -90.0f32),
        (rect.right_bottom() + vec2(-radius, -radius), 0.0),
        (rect.left_bottom() + vec2(radius, -radius), 90.0),
        (rect.left_top() + vec2(radius, radius), 180.0),
    ];
    const STEPS: usize = 8;
    for (c, start) in corners {
        for i in 0..=STEPS {
            let ang = (start + 90.0 * i as f32 / STEPS as f32).to_radians();
            let p = c + radius * vec2(ang.cos(), ang.sin());
            m.colored_vertex(p, col(p.x));
        }
    }
    let n = (m.vertices.len() - 1) as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    painter.add(egui::Shape::mesh(m));
}

/// Status dot with a soft glow; `pulse` in 0..=1 sets the halo strength.
pub fn glow_dot(painter: &Painter, center: Pos2, radius: f32, color: Color32, pulse: f32) {
    let glow = Shadow {
        offset: [0, 0],
        blur: (8.0 + 6.0 * pulse) as u8,
        spread: 2,
        color: color.gamma_multiply(0.30 + 0.22 * pulse),
    };
    let r = Rect::from_center_size(center, Vec2::splat(radius * 1.4));
    painter.add(glow.as_shape(r, CornerRadius::same(3)));
    painter.circle_filled(center, radius, color);
}

/// Static halo strength for status dots (no animation: idle frames must stay free).
pub const GLOW_REST: f32 = 0.5;

/// Mix `a` toward `b` by `t` in gamma space.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Menus hover with `widgets.hovered.weak_bg_fill` on top of `window_fill`; if they match,
    /// the highlighted item is invisible (egui-style.md §1.2).
    #[test]
    fn menu_hover_is_distinguishable_in_both_variants() {
        for (pal, dark) in [(&DARK, true), (&LIGHT, false)] {
            let mut style = Style::default();
            build(&mut style, pal, dark);
            assert_ne!(
                style.visuals.window_fill, style.visuals.widgets.hovered.weak_bg_fill,
                "dark={dark}"
            );
            assert_eq!(style.visuals.dark_mode, dark);
        }
    }

    #[test]
    fn menu_style_keeps_roomy_padding() {
        let mut style = Style::default();
        menu_style(&mut style);
        assert!(style.spacing.button_padding.x >= 8.0);
        assert_eq!(
            style.visuals.widgets.inactive.weak_bg_fill,
            Color32::TRANSPARENT
        );
    }
}
