//! Chrome theme: palettes, egui `Style`/`Visuals` and the small painting helpers shared by every
//! chrome widget. Two looks (`UiStyle::Neutral`, flat zinc greys with a teal accent, and
//! `UiStyle::Neon`, "Midnight neon") × two themes (dark/light) × two densities.
//!
//! The terminal grid keeps its own colors (`deck_term::TermTheme`); this only styles the chrome.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

use deck_core::config::{Density, UiStyle};
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
    /// Decorative icon colour (menu/row icons): accent in Neon, a quiet grey in Neutral.
    pub glyph: Color32,
    /// Gradient start (violet); equals `accent` in Neutral.
    pub accent_a: Color32,
    /// Gradient end (cyan); equals `accent` in Neutral.
    pub accent_b: Color32,
    /// Solid accent for strokes, selection, focus.
    pub accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
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
    glyph: Color32::from_rgb(124, 108, 255),
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
    glyph: Color32::from_rgb(110, 84, 240),
    accent_a: Color32::from_rgb(124, 77, 235),
    accent_b: Color32::from_rgb(8, 163, 200),
    accent: Color32::from_rgb(110, 84, 240),
    ok: Color32::from_rgb(16, 160, 110),
    warn: Color32::from_rgb(200, 130, 0),
    danger: Color32::from_rgb(214, 54, 54),
};

const NEUTRAL_DARK: Palette = Palette {
    bg: rgb(0x0b0b0d),
    panel: rgb(0x0f0f11),
    bar: rgb(0x0f0f11),
    surface: rgb(0x17171a),
    raised: rgb(0x1f1f23),
    stroke: rgb(0x232327),
    text: rgb(0xd4d4d8),
    text_strong: rgb(0xfafafa),
    weak: rgb(0x71717a),
    glyph: rgb(0xa1a1aa),
    accent_a: rgb(0x2dd4bf),
    accent_b: rgb(0x2dd4bf),
    accent: rgb(0x2dd4bf),
    ok: rgb(0x4ade80),
    warn: rgb(0xfbbf24),
    danger: rgb(0xf87171),
};

const NEUTRAL_LIGHT: Palette = Palette {
    bg: rgb(0xffffff),
    panel: rgb(0xfafafa),
    bar: rgb(0xfafafa),
    surface: rgb(0xf4f4f5),
    raised: rgb(0xe4e4e7),
    stroke: rgb(0xe4e4e7),
    text: rgb(0x3f3f46),
    text_strong: rgb(0x09090b),
    weak: rgb(0x71717a),
    glyph: rgb(0x71717a),
    accent_a: rgb(0x0d9488),
    accent_b: rgb(0x0d9488),
    accent: rgb(0x0d9488),
    ok: rgb(0x16a34a),
    warn: rgb(0xca8a04),
    danger: rgb(0xdc2626),
};

static STYLE: AtomicU8 = AtomicU8::new(0);
static COMPACT: AtomicU8 = AtomicU8::new(1);

/// Whether the Neutral look is active (flat, hairlines, teal accent, Geist).
pub fn neutral() -> bool {
    STYLE.load(Ordering::Relaxed) == 0
}

/// Whether the Compact density is active.
pub fn compact() -> bool {
    COMPACT.load(Ordering::Relaxed) != 0
}

/// `compact` or `comfortable` metric depending on the active density.
pub fn dens(compact_px: f32, comfortable_px: f32) -> f32 {
    if compact() {
        compact_px
    } else {
        comfortable_px
    }
}

/// Family of the chrome's proportional text: Geist in Neutral, the system UI font in Neon.
pub fn ui_family() -> FontFamily {
    static NEUTRAL_NAMED: LazyLock<FontFamily> =
        LazyLock::new(|| FontFamily::Name(crate::ui_fonts::NEUTRAL_FAMILY.into()));
    if neutral() {
        NEUTRAL_NAMED.clone()
    } else {
        FontFamily::Proportional
    }
}

/// Proportional UI font of `size` points in the active style's family (for painted/galley text).
pub fn font(size: f32) -> FontId {
    FontId::new(size, ui_family())
}

impl Palette {
    pub fn of(dark: bool) -> &'static Palette {
        match (neutral(), dark) {
            (true, true) => &NEUTRAL_DARK,
            (true, false) => &NEUTRAL_LIGHT,
            (false, true) => &DARK,
            (false, false) => &LIGHT,
        }
    }

    pub fn of_ui(ui: &Ui) -> &'static Palette {
        Self::of(ui.visuals().dark_mode)
    }

    pub fn of_ctx(ctx: &Context) -> &'static Palette {
        Self::of(ctx.global_style().visuals.dark_mode)
    }
}

/// Applies style × density × theme: rebuilds both egui style slots and selects the variant.
pub fn apply(ctx: &Context, style: UiStyle, density: Density, dark: bool) {
    STYLE.store(u8::from(style == UiStyle::Neon), Ordering::Relaxed);
    COMPACT.store(u8::from(density == Density::Compact), Ordering::Relaxed);
    let neutral = style == UiStyle::Neutral;
    let (pd, pl) = if neutral {
        (&NEUTRAL_DARK, &NEUTRAL_LIGHT)
    } else {
        (&DARK, &LIGHT)
    };
    ctx.style_mut_of(Theme::Dark, |s| build(s, pd, true, neutral, density));
    ctx.style_mut_of(Theme::Light, |s| build(s, pl, false, neutral, density));
    ctx.set_theme(if dark {
        ThemePreference::Dark
    } else {
        ThemePreference::Light
    });
}

fn build(style: &mut Style, p: &Palette, dark: bool, neutral: bool, density: Density) {
    let compact = density == Density::Compact;
    let family = if neutral {
        FontFamily::Name(crate::ui_fonts::NEUTRAL_FAMILY.into())
    } else {
        FontFamily::Proportional
    };
    let prop = |size| FontId::new(size, family.clone());
    let (body, small, section, title, heading) = if neutral {
        (13.0, 12.0, 10.5, 14.0, 18.0)
    } else {
        (14.0, 12.0, 11.5, 15.0, 22.0)
    };
    style.text_styles = [
        (TextStyle::Small, prop(small)),
        (TextStyle::Body, prop(body)),
        (TextStyle::Button, prop(body)),
        (TextStyle::Heading, prop(heading)),
        (
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
        (TextStyle::Name("Section".into()), prop(section)),
        (TextStyle::Name("Title".into()), prop(title)),
    ]
    .into();
    style.animation_time = if neutral { 0.08 } else { 0.12 };
    style.interaction.tooltip_delay = 0.4;

    let sp = &mut style.spacing;
    let (item, pad, interact) = match (neutral, compact) {
        (_, true) => (vec2(6.0, 4.0), vec2(8.0, 4.0), vec2(32.0, 24.0)),
        (true, false) => (vec2(8.0, 6.0), vec2(12.0, 6.0), vec2(36.0, 30.0)),
        (false, false) => (vec2(8.0, 6.0), vec2(12.0, 6.0), vec2(36.0, 30.0)),
    };
    sp.item_spacing = item;
    sp.button_padding = pad;
    sp.interact_size = interact;
    sp.menu_margin = Margin::same(if compact { 4 } else { 6 });
    sp.window_margin = Margin::same(if compact { 12 } else { 16 });
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
    v.window_fill = match (neutral, dark) {
        (true, true) => p.panel,
        (true, false) => Color32::WHITE,
        (false, true) => Color32::from_rgb(18, 24, 44),
        (false, false) => Color32::WHITE,
    };
    v.extreme_bg_color = if neutral {
        p.bg
    } else {
        mix(p.bg, p.raised, 0.55)
    };
    v.text_edit_bg_color = Some(if neutral { p.bg } else { p.surface });
    v.faint_bg_color = p.surface;
    v.code_bg_color = p.surface;
    v.hyperlink_color = p.accent_b;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    if neutral {
        v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.28 } else { 0.22 });
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.text_cursor.stroke = Stroke::new(1.5, p.text_strong);
    } else {
        v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.35 } else { 0.28 });
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.text_cursor.stroke = Stroke::new(2.0, p.accent_b);
    }
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.window_corner_radius = CornerRadius::same(if neutral { 10 } else { 14 });
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, p.stroke);
    let shadow = |dy: i8, blur: u8, a: u8| Shadow {
        offset: [0, dy],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(if dark { a } else { a / 3 }),
    };
    if neutral {
        v.window_shadow = shadow(8, 24, 120);
        v.popup_shadow = shadow(4, 16, 100);
    } else {
        v.window_shadow = shadow(12, 32, 150);
        v.popup_shadow = shadow(6, 22, 130);
    }

    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;

    if neutral {
        let hover_stroke = mix(p.stroke, p.weak, 0.35);
        let (rest, hover) = if dark {
            (p.surface, p.raised)
        } else {
            (p.bg, p.surface)
        };
        w.inactive.bg_fill = rest;
        w.inactive.weak_bg_fill = rest;
        w.inactive.bg_stroke = Stroke::new(1.0, p.stroke);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.hovered.bg_fill = hover;
        w.hovered.weak_bg_fill = hover;
        w.hovered.bg_stroke = Stroke::new(1.0, hover_stroke);
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_strong);
        w.active.bg_fill = p.raised;
        w.active.weak_bg_fill = p.raised;
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.0, p.text_strong);
        w.open.bg_fill = p.raised;
        w.open.weak_bg_fill = p.raised;
        w.open.bg_stroke = Stroke::new(1.0, p.stroke);
        w.open.fg_stroke = Stroke::new(1.0, p.text);
    } else {
        w.inactive.bg_fill = p.raised;
        w.inactive.weak_bg_fill = p.surface;
        w.inactive.bg_stroke = Stroke::new(1.0, p.stroke);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.hovered.bg_fill = p.raised;
        w.hovered.weak_bg_fill = p.raised;
        w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
        w.hovered.fg_stroke = Stroke::new(1.5, p.text_strong);
        w.active.bg_fill = p.accent.gamma_multiply(0.5);
        w.active.weak_bg_fill = p.accent.gamma_multiply(0.5);
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.5, p.text_strong);
        w.open.bg_fill = p.raised;
        w.open.weak_bg_fill = p.raised;
        w.open.bg_stroke = Stroke::new(1.0, p.stroke);
        w.open.fg_stroke = Stroke::new(1.0, p.text);
    }
    w.inactive.corner_radius = r;
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open.corner_radius = r;
}

/// egui re-applies `menu_style` (button padding 2×0, transparent inactive fill, no strokes) to every
/// menu and submenu; this keeps that look but restores item padding for the active density.
pub fn menu_style(style: &mut Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding = vec2(dens(8.0, 10.0), dens(4.0, 6.0));
    style.spacing.item_spacing.y = 2.0;
    style.spacing.interact_size.y = dens(24.0, 28.0);
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
    RichText::new(text).variation("wght", if neutral() { 600.0 } else { 640.0 })
}

/// Small uppercase caption used above groups (tracked out in Neutral).
pub fn section_caption(text: &str, color: Color32) -> RichText {
    let t = RichText::new(text.to_uppercase())
        .text_style(TextStyle::Name("Section".into()))
        .color(color);
    if neutral() {
        t.variation("wght", 500.0).extra_letter_spacing(0.9)
    } else {
        t.variation("wght", 700.0)
    }
}

/// Bar background at horizontal position `t` (0..=1): the Neon violet→cyan wash, flat in Neutral.
pub fn bar_tint(p: &Palette, t: f32) -> Color32 {
    if neutral() {
        p.bar
    } else {
        mix(
            mix(p.bar, p.accent_a, 0.10),
            mix(p.bar, p.accent_b, 0.05),
            t,
        )
    }
}

/// Fills `rect` with the bar background.
pub fn paint_bar(painter: &Painter, rect: Rect, p: &Palette) {
    if neutral() {
        painter.rect_filled(rect, 0, p.bar);
    } else {
        gradient_rect(painter, rect, bar_tint(p, 0.0), bar_tint(p, 1.0), true);
    }
}

/// Hover / quiet-selection fill for list rows and ghost buttons (Neutral: surface; Neon: raised).
pub fn hover_fill(p: &Palette) -> Color32 {
    if neutral() { p.surface } else { p.raised }
}

/// Fill of an active / selected control: the raised surface in Neutral, an `alpha` accent wash in Neon.
pub fn accent_fill(p: &Palette, neon_alpha: f32) -> Color32 {
    if neutral() {
        p.raised
    } else {
        p.accent.gamma_multiply(neon_alpha)
    }
}

/// Readable ink for text drawn on a solid `fill`: near-black on light fills, white on dark ones.
pub fn ink_on(fill: Color32) -> Color32 {
    let luma =
        0.299 * f32::from(fill.r()) + 0.587 * f32::from(fill.g()) + 0.114 * f32::from(fill.b());
    if luma > 150.0 {
        rgb(0x09_09_0b)
    } else {
        Color32::WHITE
    }
}

/// Ink drawn on a solid `accent` fill (near-black on Neutral-dark teal, white elsewhere).
pub fn on_accent(p: &Palette) -> Color32 {
    ink_on(p.accent)
}

/// `neutral_px` in Neutral, `neon_px` in Neon (corner radii that differ between the looks).
pub fn rad(neutral_px: u8, neon_px: u8) -> u8 {
    if neutral() { neutral_px } else { neon_px }
}

/// Corner radius of popups, popovers and dialogs: 10 in Neutral, 16 in Neon.
pub fn popup_radius() -> u8 {
    rad(10, 16)
}

/// Border of a floating surface: the plain hairline in Neutral, an accent wash in Neon.
pub fn popup_stroke(p: &Palette) -> Stroke {
    if neutral() {
        Stroke::new(1.0, p.stroke)
    } else {
        Stroke::new(1.0, p.accent.gamma_multiply(0.45))
    }
}

/// Border of a status-coloured card (toast, banner): hairline in Neutral, `color` wash in Neon.
pub fn status_stroke(p: &Palette, color: Color32, neon_alpha: f32) -> Stroke {
    if neutral() {
        Stroke::new(1.0, p.stroke)
    } else {
        Stroke::new(1.0, color.gamma_multiply(neon_alpha))
    }
}

/// `base` washed toward the accent by `t` in Neon; untouched in Neutral (flat, no accent fills).
pub fn accent_wash(base: Color32, accent: Color32, t: f32) -> Color32 {
    if neutral() {
        base
    } else {
        mix(base, accent, t)
    }
}

/// Dimming layer behind modal dialogs and popups.
pub fn backdrop(dark: bool) -> Color32 {
    Color32::from_black_alpha(match (neutral(), dark) {
        (true, true) => 140,
        (true, false) => 60,
        (false, true) => 150,
        (false, false) => 70,
    })
}

/// Shadow under modal dialogs and large popups (soft and short in Neutral).
pub fn modal_shadow(dark: bool) -> Shadow {
    let (dy, blur, a) = if neutral() {
        (10, 32, 150)
    } else {
        (20, 48, 180)
    };
    Shadow {
        offset: [0, dy],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(if dark { a } else { a / 3 }),
    }
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
    if a == b {
        painter.rect_filled(rect, radius.min(255.0) as u8, a);
        return;
    }
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

/// Status dot (with a soft glow in Neon, flat in Neutral); `pulse` in 0..=1 sets the halo strength.
pub fn glow_dot(painter: &Painter, center: Pos2, radius: f32, color: Color32, pulse: f32) {
    if neutral() {
        painter.circle_filled(center, radius, color);
        return;
    }
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

    const VARIANTS: [(&Palette, bool, bool); 4] = [
        (&DARK, true, false),
        (&LIGHT, false, false),
        (&NEUTRAL_DARK, true, true),
        (&NEUTRAL_LIGHT, false, true),
    ];

    /// Menus hover with `widgets.hovered.weak_bg_fill` on top of `window_fill`; if they match,
    /// the highlighted item is invisible (egui-style.md §1.2).
    #[test]
    fn menu_hover_is_distinguishable_in_all_variants() {
        for (pal, dark, neutral) in VARIANTS {
            for density in [Density::Compact, Density::Comfortable] {
                let mut style = Style::default();
                build(&mut style, pal, dark, neutral, density);
                assert_ne!(
                    style.visuals.window_fill, style.visuals.widgets.hovered.weak_bg_fill,
                    "dark={dark} neutral={neutral}"
                );
                assert_eq!(style.visuals.dark_mode, dark);
            }
        }
    }

    #[test]
    fn neutral_palettes_are_flat_and_match_the_contract() {
        for pal in [&NEUTRAL_DARK, &NEUTRAL_LIGHT] {
            assert_eq!(pal.accent_a, pal.accent);
            assert_eq!(pal.accent_b, pal.accent);
        }
        assert_eq!(NEUTRAL_DARK.bg, Color32::from_rgb(0x0b, 0x0b, 0x0d));
        assert_eq!(NEUTRAL_DARK.accent, Color32::from_rgb(0x2d, 0xd4, 0xbf));
        assert_eq!(NEUTRAL_LIGHT.accent, Color32::from_rgb(0x0d, 0x94, 0x88));
    }

    #[test]
    fn menu_style_keeps_item_padding() {
        let mut style = Style::default();
        menu_style(&mut style);
        assert!(style.spacing.button_padding.x >= 8.0);
        assert_eq!(
            style.visuals.widgets.inactive.weak_bg_fill,
            Color32::TRANSPARENT
        );
    }

    #[test]
    fn compact_is_tighter_than_comfortable() {
        let mut c = Style::default();
        let mut f = Style::default();
        build(&mut c, &NEUTRAL_DARK, true, true, Density::Compact);
        build(&mut f, &NEUTRAL_DARK, true, true, Density::Comfortable);
        assert!(c.spacing.interact_size.y < f.spacing.interact_size.y);
        assert!(c.spacing.item_spacing.y < f.spacing.item_spacing.y);
    }
}
