//! Chrome theme: palettes, egui `Style`/`Visuals` and the small painting helpers shared by every
//! chrome widget. Eighteen styles (`UiStyle`): Neutral (flat zinc greys, teal accent), Neon
//! ("Midnight neon") and sixteen dark-only gallery styles, each a palette plus a structural
//! [`Look`]; × two themes (dark/light, Neutral and Neon only) × two densities.
//!
//! The terminal grid keeps its own colors (`deck_term::TermTheme`); this only styles the chrome.

pub mod acc;
mod look;
mod palettes;
pub mod skin;

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

use deck_core::config::{Density, UiStyle};
use egui::epaint::Mesh;
use egui::style::{HandleShape, ScrollStyle};
use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Painter, Pos2, Rect, RichText,
    Shadow, Stroke, Style, TextStyle, Theme, ThemePreference, Ui, Vec2, vec2,
};

pub use look::{
    Acc, Active, Border, Canvas, CardShadow, ChevInk, Hover, Look, Rail, Region, Sel, SubBorder,
    TabKind, Tone,
};
pub use palettes::Palette;

use palettes::rgb;

/// Index into [`UiStyle::ALL`] of the active style.
static STYLE: AtomicU8 = AtomicU8::new(0);
static COMPACT: AtomicU8 = AtomicU8::new(1);

/// The structure of every style, in [`UiStyle::ALL`] order.
static LOOKS: LazyLock<[Look; 18]> = LazyLock::new(|| UiStyle::ALL.map(look::look_of));

/// The active style.
pub fn style() -> UiStyle {
    UiStyle::ALL
        .get(usize::from(STYLE.load(Ordering::Relaxed)))
        .copied()
        .unwrap_or_default()
}

/// Structure of the active style.
pub fn look() -> &'static Look {
    &LOOKS[usize::from(STYLE.load(Ordering::Relaxed)).min(LOOKS.len() - 1)]
}

/// Structure of `style` (for previews of styles that are not active).
pub fn look_of(style: UiStyle) -> Look {
    look::look_of(style)
}

/// Whether a flat-family look is active: every style but Neon (flat, hairlines, JetBrains Mono).
pub fn flat() -> bool {
    style() != UiStyle::Neon
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

/// Family of the chrome's UI text: JetBrains Mono in the flat styles, the system UI font in
/// Neon, the monospace family for the whole chrome in Phosphor.
pub fn ui_family() -> FontFamily {
    static NEUTRAL_NAMED: LazyLock<FontFamily> =
        LazyLock::new(|| FontFamily::Name(crate::ui_fonts::NEUTRAL_FAMILY.into()));
    if look().mono_ui {
        FontFamily::Monospace
    } else if flat() {
        NEUTRAL_NAMED.clone()
    } else {
        FontFamily::Proportional
    }
}

/// Proportional UI font of `size` points in the active style's family (for painted/galley text).
pub fn font(size: f32) -> FontId {
    FontId::new(size, ui_family())
}

/// Font of labels, badges, meters, paths and ids: monospace where the style sets labels in mono
/// (Blueprint, Phosphor), else the UI font.
pub fn label_font(size: f32) -> FontId {
    if look().mono_labels {
        FontId::new(size, FontFamily::Monospace)
    } else {
        font(size)
    }
}

/// Radius of rows, chips, fields and pills for a `base` radius of the flat look: the style's
/// own radius where it is tighter (Blueprint 3, Phosphor 2), unchanged elsewhere.
pub fn ui_rad(base: u8) -> u8 {
    let r = look().ui_r;
    if r >= 8 { base } else { base.min(r) }
}

/// Why the Light theme is unavailable for the dark-only `style` (`None` for Neutral and Neon).
pub fn dark_only_reason(style: UiStyle) -> Option<&'static str> {
    Some(match style {
        UiStyle::Neutral | UiStyle::Neon => return None,
        UiStyle::ZincContrast => "Zinc Contrast is dark-only",
        UiStyle::LayeredDepth => "Layered Depth is dark-only",
        UiStyle::TealSignal => "Teal Signal is dark-only",
        UiStyle::SlateInk => "Slate Ink is dark-only",
        UiStyle::MonoWire => "Mono Wire is dark-only",
        UiStyle::GraphiteIslands => "Graphite Islands is dark-only",
        UiStyle::Aurora => "Aurora is dark-only",
        UiStyle::ObsidianGlass => "Obsidian Glass is dark-only",
        UiStyle::Blueprint => "Blueprint is dark-only",
        UiStyle::Ember => "Ember is dark-only",
        UiStyle::IndigoNight => "Indigo Night is dark-only",
        UiStyle::Phosphor => "Phosphor is dark-only",
        UiStyle::NeoBrutal => "Neo-Brutal is dark-only",
        UiStyle::RoseNoir => "Rosé Noir is dark-only",
        UiStyle::Spotlight => "Spotlight is dark-only",
        UiStyle::TwoToneFrame => "Two-Tone Frame is dark-only",
    })
}

/// Whether the active style is one of the sixteen gallery styles (not Neutral or Neon).
pub fn is_gallery() -> bool {
    !matches!(style(), UiStyle::Neutral | UiStyle::Neon)
}

/// Palette of `style` regardless of the active one (previews, terminal colours).
pub fn palette(style: UiStyle, dark: bool) -> &'static Palette {
    palettes::of(style, dark)
}

impl Palette {
    /// Palette of the active style; `dark` only matters for Neutral and Neon.
    pub fn of(dark: bool) -> &'static Palette {
        palettes::of(style(), dark)
    }

    pub fn of_ui(ui: &Ui) -> &'static Palette {
        Self::of(ui.visuals().dark_mode)
    }

    pub fn of_ctx(ctx: &Context) -> &'static Palette {
        Self::of(ctx.global_style().visuals.dark_mode)
    }
}

/// Applies style × density × theme: rebuilds both egui style slots and selects the variant.
/// `dark` is `AppConfig::dark()` (always true for dark-only styles).
pub fn apply(ctx: &Context, style: UiStyle, density: Density, dark: bool) {
    let idx = UiStyle::ALL.iter().position(|s| *s == style).unwrap_or(0);
    STYLE.store(idx as u8, Ordering::Relaxed);
    COMPACT.store(u8::from(density == Density::Compact), Ordering::Relaxed);
    let flat = style != UiStyle::Neon;
    let look = look::look_of(style);
    let (pd, pl) = (palettes::of(style, true), palettes::of(style, false));
    ctx.style_mut_of(Theme::Dark, |s| build(s, pd, true, flat, density, &look));
    ctx.style_mut_of(Theme::Light, |s| build(s, pl, false, flat, density, &look));
    ctx.set_theme(if dark || style.dark_only() {
        ThemePreference::Dark
    } else {
        ThemePreference::Light
    });
}

fn build(style: &mut Style, p: &Palette, dark: bool, neutral: bool, density: Density, look: &Look) {
    let compact = density == Density::Compact;
    let family = if look.mono_ui {
        FontFamily::Monospace
    } else if neutral {
        FontFamily::Name(crate::ui_fonts::NEUTRAL_FAMILY.into())
    } else {
        FontFamily::Proportional
    };
    let prop = |size| FontId::new(size, family.clone());
    let (body, small, section, title, heading) = if look.mono_ui {
        (11.5, 10.5, 10.0, 12.5, 16.0)
    } else if neutral {
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
    // `menu_margin` is the inner margin egui gives every `Frame::popup`: tooltips and hover cards.
    // Menus get their own tighter margin in [`menu_style`].
    sp.menu_margin = if compact {
        Margin::symmetric(10, 8)
    } else {
        Margin::symmetric(12, 10)
    };
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
    v.window_corner_radius = CornerRadius::same(if neutral { popup_radius_of(look) } else { 14 });
    v.menu_corner_radius = CornerRadius::same(popup_radius_of(look).min(10));
    v.window_stroke = Stroke::new(look.card.border_w.min(2.0), p.stroke);
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

    let r = CornerRadius::same(if neutral { 8.min(look.ui_r) } else { 8 });
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(look.sep_w, p.separator);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;

    if neutral {
        let hover_stroke = hover_stroke(p);
        let bw = look.card.border_w.min(2.0);
        let (rest, hover) = if dark {
            (p.surface, p.hover_bg)
        } else {
            (p.bg, p.surface)
        };
        w.inactive.bg_fill = rest;
        w.inactive.weak_bg_fill = rest;
        w.inactive.bg_stroke = Stroke::new(bw, p.stroke);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.hovered.bg_fill = hover;
        w.hovered.weak_bg_fill = hover;
        w.hovered.bg_stroke = Stroke::new(bw, hover_stroke);
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_strong);
        w.active.bg_fill = p.raised;
        w.active.weak_bg_fill = p.raised;
        w.active.bg_stroke = Stroke::new(bw, p.accent);
        w.active.fg_stroke = Stroke::new(1.0, p.text_strong);
        w.open.bg_fill = p.raised;
        w.open.weak_bg_fill = p.raised;
        w.open.bg_stroke = Stroke::new(bw, p.stroke);
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
    style.spacing.menu_margin = Margin::same(if compact() { 6 } else { 8 });
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
    RichText::new(text).variation("wght", if flat() { 600.0 } else { 640.0 })
}

/// Small uppercase caption used above groups (tracked out in Neutral).
pub fn section_caption(text: &str, color: Color32) -> RichText {
    let t = RichText::new(text.to_uppercase())
        .text_style(TextStyle::Name("Section".into()))
        .color(color);
    let t = if look().mono_labels {
        t.family(FontFamily::Monospace)
    } else {
        t
    };
    if flat() {
        t.variation("wght", 500.0).extra_letter_spacing(0.9)
    } else {
        t.variation("wght", 700.0)
    }
}

/// Reading style for egui_commonmark text (apply inside a `ui.scope`): headings a step above the
/// body rather than the app's title size, drawn in `text_strong` (the viewer paints headings and
/// **strong** in the "strong" text colour), inline code on `raised` so it stands out on a
/// `surface` card, monospace a notch below the body, a little air between wrapped rows.
pub fn markdown_style(style: &mut Style, p: &Palette) {
    let body = style
        .text_styles
        .get(&TextStyle::Body)
        .map_or(13.0, |f| f.size);
    if let Some(h) = style.text_styles.get_mut(&TextStyle::Heading) {
        h.size = body + 5.0;
    }
    if let Some(m) = style.text_styles.get_mut(&TextStyle::Monospace) {
        m.size = body - 1.0;
    }
    style.spacing.item_spacing.y = 3.0;
    style.visuals.code_bg_color = p.raised;
    style.visuals.widgets.active.fg_stroke.color = p.text_strong;
}

/// Bar background at horizontal position `t` (0..=1): the Neon violet→cyan wash, flat in Neutral.
pub fn bar_tint(p: &Palette, t: f32) -> Color32 {
    if flat() {
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
    if flat() {
        painter.rect_filled(rect, 0, p.bar);
    } else {
        gradient_rect(painter, rect, bar_tint(p, 0.0), bar_tint(p, 1.0), true);
    }
}

/// Hover / quiet-selection fill for list rows and ghost buttons.
pub fn hover_fill(p: &Palette) -> Color32 {
    p.hover_bg
}

/// Outline of a hovered row, tab, card or button: clearly brighter than the resting hairline.
pub fn hover_stroke(p: &Palette) -> Color32 {
    p.hover_line
}

/// Fill of an active / selected control: the raised surface in Neutral, an `alpha` accent wash in Neon.
pub fn accent_fill(p: &Palette, neon_alpha: f32) -> Color32 {
    if flat() {
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

/// `neutral_px` in the flat styles, `neon_px` in Neon (corner radii that differ between the looks).
pub fn rad(neutral_px: u8, neon_px: u8) -> u8 {
    if flat() { ui_rad(neutral_px) } else { neon_px }
}

/// Inner margin of modal dialogs (spacing, not a radius: never derive it from [`rad`]).
pub fn dialog_margin() -> Margin {
    Margin::same(if !flat() {
        22
    } else if compact() {
        18
    } else {
        20
    })
}

/// Popup radius of a style's look: its card radius where tighter than 10 (Blueprint 3, Phosphor 2,
/// Neo-Brutal 6), else 10.
fn popup_radius_of(look: &Look) -> u8 {
    look.card.radius.clamp(3, 10)
}

/// Corner radius of popups, popovers and dialogs: 10 in the flat styles (tighter in Blueprint,
/// Phosphor and Neo-Brutal), 16 in Neon.
pub fn popup_radius() -> u8 {
    if flat() { popup_radius_of(look()) } else { 16 }
}

/// Border of a floating surface: the plain hairline in Neutral, an accent wash in Neon.
pub fn popup_stroke(p: &Palette) -> Stroke {
    if flat() {
        Stroke::new(1.0, p.stroke)
    } else {
        Stroke::new(1.0, p.accent.gamma_multiply(0.45))
    }
}

/// Border of a status-coloured card (toast, banner): hairline in Neutral, `color` wash in Neon.
pub fn status_stroke(p: &Palette, color: Color32, neon_alpha: f32) -> Stroke {
    if flat() {
        Stroke::new(1.0, p.stroke)
    } else {
        Stroke::new(1.0, color.gamma_multiply(neon_alpha))
    }
}

/// `base` washed toward the accent by `t` in Neon; untouched in Neutral (flat, no accent fills).
pub fn accent_wash(base: Color32, accent: Color32, t: f32) -> Color32 {
    if flat() { base } else { mix(base, accent, t) }
}

/// Dimming layer behind modal dialogs and popups.
pub fn backdrop(dark: bool) -> Color32 {
    Color32::from_black_alpha(match (flat(), dark) {
        (true, true) => 140,
        (true, false) => 60,
        (false, true) => 150,
        (false, false) => 70,
    })
}

/// Shadow under modal dialogs and large popups (soft and short in Neutral).
pub fn modal_shadow(dark: bool) -> Shadow {
    let (dy, blur, a) = if flat() { (10, 32, 150) } else { (20, 48, 180) };
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
    if flat() {
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
    use super::palettes::of as palette_of;
    use super::*;

    /// Menus hover with `widgets.hovered.weak_bg_fill` on top of `window_fill`; if they match,
    /// the highlighted item is invisible (egui-style.md §1.2).
    #[test]
    fn menu_hover_is_distinguishable_in_all_variants() {
        for ui_style in UiStyle::ALL {
            for dark in [true, false] {
                if !dark && ui_style.dark_only() {
                    continue;
                }
                let pal = palettes::of(ui_style, dark);
                let look = look::look_of(ui_style);
                for density in [Density::Compact, Density::Comfortable] {
                    let mut style = Style::default();
                    build(
                        &mut style,
                        pal,
                        dark,
                        ui_style != UiStyle::Neon,
                        density,
                        &look,
                    );
                    assert_ne!(
                        style.visuals.window_fill, style.visuals.widgets.hovered.weak_bg_fill,
                        "{ui_style:?} dark={dark}"
                    );
                    assert_eq!(style.visuals.dark_mode, dark);
                }
            }
        }
    }

    #[test]
    fn converted_palettes_keep_the_core_values() {
        let (dark, light) = (
            palette_of(UiStyle::Neutral, true),
            palette_of(UiStyle::Neutral, false),
        );
        for pal in [dark, light] {
            assert_eq!(pal.accent_a, pal.accent);
            assert_eq!(pal.accent_b, pal.accent);
        }
        assert_eq!(dark.bg, Color32::from_rgb(0x0b, 0x0b, 0x0d));
        assert_eq!(dark.accent, Color32::from_rgb(0x2d, 0xd4, 0xbf));
        assert_eq!(light.accent, Color32::from_rgb(0x0d, 0x94, 0x88));
        let neon = palette_of(UiStyle::Neon, true);
        assert_eq!(
            neon.hover_line,
            Color32::from_rgba_premultiplied(74, 65, 153, 153)
        );
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
        let look = look::look_of(UiStyle::Neutral);
        let pal = palette_of(UiStyle::Neutral, true);
        build(&mut c, pal, true, true, Density::Compact, &look);
        build(&mut f, pal, true, true, Density::Comfortable, &look);
        assert!(c.spacing.interact_size.y < f.spacing.interact_size.y);
        assert!(c.spacing.item_spacing.y < f.spacing.item_spacing.y);
    }

    #[test]
    fn dark_only_styles_ignore_the_requested_theme() {
        for style in UiStyle::ALL.into_iter().filter(|s| s.dark_only()) {
            assert_eq!(
                palettes::of(style, true).bg,
                palettes::of(style, false).bg,
                "{style:?}"
            );
            assert!(dark_only_reason(style).is_some(), "{style:?}");
        }
        for style in UiStyle::ALL.into_iter().filter(|s| !s.dark_only()) {
            assert!(dark_only_reason(style).is_none(), "{style:?}");
        }
    }

    #[test]
    fn hover_is_visible_in_every_style() {
        for style in UiStyle::ALL {
            for dark in [true, false] {
                if !dark && style.dark_only() {
                    continue;
                }
                let p = palettes::of(style, dark);
                assert_ne!(p.hover_line, p.stroke, "{style:?} dark={dark}");
                assert_ne!(p.hover_line, p.surface, "{style:?} dark={dark}");
                assert_ne!(p.hover_bg, p.panel, "{style:?} dark={dark}");
                assert_ne!(p.raised, p.surface, "{style:?} dark={dark}");
            }
        }
    }
}
