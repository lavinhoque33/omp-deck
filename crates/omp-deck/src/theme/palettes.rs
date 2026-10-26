//! Colour tokens of every style as egui colours. The values live in `deck_core::theme` (the single
//! authoritative table, shared with the headless gateway); this module converts each palette once.

use std::sync::LazyLock;

use deck_core::config::UiStyle;
use deck_core::theme::{self as core, Rgba};
use egui::Color32;

/// Colour tokens for one theme variant.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Terminal surround / the canvas behind the dashboard, start screen and settings.
    pub bg: Color32,
    /// Sidebar and page background.
    pub panel: Color32,
    /// Title bar, tab row and status bar.
    pub bar: Color32,
    /// Window backdrop: what shows in the gaps of the Islands layout and around the Two-Tone frame.
    pub window: Color32,
    /// Cards, inputs (top of the card gradient).
    pub surface: Color32,
    /// Bottom of the card gradient; equals `surface` for flat cards.
    pub surface_end: Color32,
    /// Hover fill, popups' highlighted row, chips, selected rows.
    pub raised: Color32,
    /// Borders of cards, fields and rows.
    pub stroke: Color32,
    /// Region separators (sidebar | main, tab row, status bar).
    pub separator: Color32,
    /// Outline of a hovered row, tab, card or button.
    pub hover_line: Color32,
    /// Fill of a hovered row, tab, card or button.
    pub hover_bg: Color32,
    /// Subagent accordion fill.
    pub tray: Color32,
    /// Subagent accordion border and tree guides.
    pub tray_line: Color32,
    pub text: Color32,
    pub text_strong: Color32,
    pub weak: Color32,
    /// Decorative icon colour (menu/row icons): accent in Neon, a quiet grey elsewhere.
    pub glyph: Color32,
    /// Gradient start (violet); equals `accent` in the flat styles.
    pub accent_a: Color32,
    /// Gradient end (cyan); equals `accent` in the flat styles except Aurora.
    pub accent_b: Color32,
    /// Solid accent for strokes, selection, focus.
    pub accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
}

pub(super) const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Core's premultiplied token as the egui colour with identical components.
pub(super) const fn color(c: Rgba) -> Color32 {
    Color32::from_rgba_premultiplied(c.r, c.g, c.b, c.a)
}

impl From<&core::Palette> for Palette {
    fn from(p: &core::Palette) -> Palette {
        Palette {
            bg: color(p.bg),
            panel: color(p.panel),
            bar: color(p.bar),
            window: color(p.window),
            surface: color(p.surface),
            surface_end: color(p.surface_end),
            raised: color(p.raised),
            stroke: color(p.stroke),
            separator: color(p.separator),
            hover_line: color(p.hover_line),
            hover_bg: color(p.hover_bg),
            tray: color(p.tray),
            tray_line: color(p.tray_line),
            text: color(p.text),
            text_strong: color(p.text_strong),
            weak: color(p.weak),
            glyph: color(p.glyph),
            accent_a: color(p.accent_a),
            accent_b: color(p.accent_b),
            accent: color(p.accent),
            ok: color(p.ok),
            warn: color(p.warn),
            danger: color(p.danger),
        }
    }
}

/// Every palette converted once: `[style in UiStyle::ALL order][dark, light]`.
static CACHE: LazyLock<Vec<[Palette; 2]>> = LazyLock::new(|| {
    UiStyle::ALL
        .iter()
        .map(|&s| {
            [
                Palette::from(core::palette(s, true)),
                Palette::from(core::palette(s, false)),
            ]
        })
        .collect()
});

/// The palette of `style`; `dark` only matters for Neutral and Neon (every other style is dark-only).
pub(super) fn of(style: UiStyle, dark: bool) -> &'static Palette {
    // `UiStyle::ALL` lists every style; the fallback only keeps the lookup total.
    let i = UiStyle::ALL.iter().position(|&s| s == style).unwrap_or(0);
    &CACHE[i][usize::from(!dark)]
}
