//! Colour tokens of every style: the Neon and Neutral palettes (dark/light) and one dark palette per
//! gallery style, transcribed from the approved mockups (`docs/notes/theme-mockups.html`).

use deck_core::config::UiStyle;
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

/// `fg` at `alpha` (0..=255) composited over the opaque `bg`, as a hex colour.
const fn over(fg: u32, alpha: u32, bg: u32) -> u32 {
    const fn ch(fg: u32, bg: u32, alpha: u32, shift: u32) -> u32 {
        let f = (fg >> shift) & 0xff;
        let b = (bg >> shift) & 0xff;
        (f * alpha + b * (255 - alpha) + 127) / 255
    }
    (ch(fg, bg, alpha, 16) << 16) | (ch(fg, bg, alpha, 8) << 8) | ch(fg, bg, alpha, 0)
}

/// Token order of [`tokens`].
const N: usize = 22;

/// Builds a palette from `[window, bar, panel, bg, surface, surface_end, raised, stroke, separator,
/// hover_line, hover_bg, tray, tray_line, text, text_strong, weak, glyph, accent, accent_b, ok,
/// warn, danger]`; `accent_a` mirrors `accent`.
const fn tokens(t: [u32; N]) -> Palette {
    Palette {
        window: rgb(t[0]),
        bar: rgb(t[1]),
        panel: rgb(t[2]),
        bg: rgb(t[3]),
        surface: rgb(t[4]),
        surface_end: rgb(t[5]),
        raised: rgb(t[6]),
        stroke: rgb(t[7]),
        separator: rgb(t[8]),
        hover_line: rgb(t[9]),
        hover_bg: rgb(t[10]),
        tray: rgb(t[11]),
        tray_line: rgb(t[12]),
        text: rgb(t[13]),
        text_strong: rgb(t[14]),
        weak: rgb(t[15]),
        glyph: rgb(t[16]),
        accent_a: rgb(t[17]),
        accent_b: rgb(t[18]),
        accent: rgb(t[17]),
        ok: rgb(t[19]),
        warn: rgb(t[20]),
        danger: rgb(t[21]),
    }
}

pub(super) const DARK: Palette = Palette {
    bg: Color32::from_rgb(8, 11, 22),
    panel: Color32::from_rgb(12, 16, 31),
    bar: Color32::from_rgb(14, 19, 37),
    window: Color32::from_rgb(8, 11, 22),
    surface: Color32::from_rgb(20, 27, 49),
    surface_end: Color32::from_rgb(20, 27, 49),
    raised: Color32::from_rgb(30, 39, 70),
    stroke: Color32::from_rgb(36, 46, 78),
    separator: Color32::from_rgb(36, 46, 78),
    // accent at 60 % alpha, as `accent.gamma_multiply(0.6)` used to give.
    hover_line: Color32::from_rgba_premultiplied(74, 65, 153, 153),
    hover_bg: Color32::from_rgb(30, 39, 70),
    tray: Color32::from_rgb(20, 27, 49),
    tray_line: Color32::from_rgb(36, 46, 78),
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

pub(super) const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(244, 246, 252),
    panel: Color32::from_rgb(236, 239, 248),
    bar: Color32::from_rgb(228, 232, 245),
    window: Color32::from_rgb(244, 246, 252),
    surface: Color32::from_rgb(252, 253, 255),
    surface_end: Color32::from_rgb(252, 253, 255),
    raised: Color32::from_rgb(222, 227, 244),
    stroke: Color32::from_rgb(207, 213, 233),
    separator: Color32::from_rgb(207, 213, 233),
    hover_line: Color32::from_rgba_premultiplied(66, 50, 144, 153),
    hover_bg: Color32::from_rgb(222, 227, 244),
    tray: Color32::from_rgb(252, 253, 255),
    tray_line: Color32::from_rgb(207, 213, 233),
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

pub(super) const NEUTRAL_DARK: Palette = Palette {
    bg: rgb(0x0b0b0d),
    panel: rgb(0x0f0f11),
    bar: rgb(0x0f0f11),
    window: rgb(0x0b0b0d),
    surface: rgb(0x18181b),
    surface_end: rgb(0x18181b),
    raised: rgb(0x26262b),
    // Hairlines must stay visible on the near-black panels (sidebar | tabs | cards).
    stroke: rgb(0x34343a),
    separator: rgb(0x34343a),
    // mix(stroke, text, 0.4)
    hover_line: rgb(0x747479),
    hover_bg: rgb(0x26262b),
    tray: rgb(0x18181b),
    tray_line: rgb(0x34343a),
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

pub(super) const NEUTRAL_LIGHT: Palette = Palette {
    bg: rgb(0xffffff),
    panel: rgb(0xfafafa),
    bar: rgb(0xfafafa),
    window: rgb(0xffffff),
    surface: rgb(0xf4f4f5),
    surface_end: rgb(0xf4f4f5),
    raised: rgb(0xe4e4e7),
    stroke: rgb(0xe4e4e7),
    separator: rgb(0xe4e4e7),
    hover_line: rgb(0xa2a2a7),
    hover_bg: rgb(0xe4e4e7),
    tray: rgb(0xf4f4f5),
    tray_line: rgb(0xe4e4e7),
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

const ZINC_CONTRAST: Palette = tokens([
    0x09090b, 0x0f0f11, 0x0f0f11, 0x09090b, 0x141417, 0x141417, 0x27272a, 0x3f3f46, 0x3f3f46,
    0xa1a1aa, 0x1f1f23, 0x0c0c0e, 0x3f3f46, 0xd4d4d8, 0xfafafa, 0x8b8b94, 0xa1a1aa, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const LAYERED_DEPTH: Palette = tokens([
    0x070709, 0x0a0a0c, 0x18181c, 0x0a0a0c, 0x1e1e23, 0x17171b, 0x2a2a30, 0x303037, 0x2c2c33,
    0x8a8a95, 0x25252b, 0x0f0f12, 0x2c2c33, 0xd4d4d8, 0xfafafa, 0x80808a, 0xa1a1aa, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const TEAL_SIGNAL: Palette = tokens([
    0x090a0a,
    0x0c0e0e,
    0x0d1010,
    0x090a0a,
    0x121515,
    0x121515,
    0x1d2324,
    0x2e3638,
    0x2e3638,
    over(0x2dd4bf, 158, 0x121515),
    0x162020,
    over(0x2dd4bf, 13, 0x121515),
    over(0x2dd4bf, 71, 0x121515),
    0xd3dbdb,
    0xf7fafa,
    0x7c8a8b,
    0x9fb0b0,
    0x2dd4bf,
    0x2dd4bf,
    0x4ade80,
    0xfbbf24,
    0xf87171,
]);

const SLATE_INK: Palette = tokens([
    0x0a0e15, 0x0c111a, 0x111826, 0x0a0e15, 0x17212f, 0x141c29, 0x203045, 0x2c3c55, 0x2c3c55,
    0x7189b8, 0x1b2638, 0x101724, 0x2c3c55, 0xd3dbe8, 0xf5f8fd, 0x7d8aa3, 0x9cabc4, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const MONO_WIRE: Palette = tokens([
    0x000000, 0x000000, 0x000000, 0x000000, 0x0a0a0a, 0x0a0a0a, 0x1a1a1a, 0x333333, 0x333333,
    0xd4d4d4, 0x111111, 0x050505, 0x333333, 0xe5e5e5, 0xffffff, 0x8a8a8a, 0xa3a3a3, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const GRAPHITE_ISLANDS: Palette = tokens([
    0x050506, 0x050506, 0x111113, 0x0d0d0f, 0x1a1a1e, 0x161619, 0x26262b, 0x2f2f35, 0x2f2f35,
    0x8a8a94, 0x1e1e22, 0x121214, 0x2f2f35, 0xd4d4d8, 0xfafafa, 0x80808a, 0xa1a1aa, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const AURORA: Palette = tokens([
    0x06060a, 0x08080d, 0x0b0b12, 0x06060a, 0x11111a, 0x0d0d15, 0x1d1d2c, 0x262638, 0x24243a,
    0xb4a2ff, 0x15151f, 0x0a0a12, 0x2e2e48, 0xd7d7e6, 0xfbfbff, 0x8686a3, 0xa7a7c4, 0xa78bfa,
    0x22d3ee, 0x4ade80, 0xfbbf24, 0xfb7185,
]);

const OBSIDIAN_GLASS: Palette = tokens([
    0x000000,
    0x000000,
    over(0xffffff, 7, 0x000000),
    0x000000,
    over(0xffffff, 14, 0x000000),
    over(0xffffff, 6, 0x000000),
    over(0xffffff, 20, 0x000000),
    over(0xffffff, 28, 0x000000),
    over(0xffffff, 26, 0x000000),
    over(0xffffff, 107, 0x000000),
    over(0xffffff, 18, 0x000000),
    over(0xffffff, 6, 0x000000),
    over(0xffffff, 26, 0x000000),
    0xdedee3,
    0xffffff,
    0x8e8e96,
    0xb0b0b8,
    0x5eead4,
    0x5eead4,
    0x4ade80,
    0xfcd34d,
    0xfb7185,
]);

const BLUEPRINT: Palette = tokens([
    0x08090b, 0x0a0b0d, 0x0b0c0f, 0x08090b, 0x0e1014, 0x0e1014, 0x1a1e25, 0x2e3540, 0x2e3540,
    0x67e8f9, 0x111620, 0x0b0d11, 0x2e3540, 0xcdd5df, 0xf3f7fb, 0x7a8696, 0x9aa7b8, 0x22d3ee,
    0x22d3ee, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const EMBER: Palette = tokens([
    0x0b0908, 0x0e0b0a, 0x13100e, 0x0b0908, 0x1a1512, 0x15110f, 0x2a221d, 0x3b3029, 0x3a2f28,
    0xd6b49a, 0x211a16, 0x110d0b, 0x3b3029, 0xe8e0d9, 0xfffaf5, 0x9b8b7e, 0xb9a797, 0xfb923c,
    0xfb923c, 0xa3e635, 0xfbbf24, 0xf87171,
]);

const INDIGO_NIGHT: Palette = tokens([
    0x07071a, 0x09091f, 0x0c0c26, 0x07071a, 0x12122f, 0x0e0e27, 0x1e1e48, 0x2b2b5e, 0x29295c,
    0xa5b4fc, 0x171740, 0x0b0b22, 0x2b2b5e, 0xd9dbff, 0xffffff, 0x8a8cc2, 0xaab0e6, 0x818cf8,
    0x818cf8, 0x34d399, 0xfbbf24, 0xfb7185,
]);

const PHOSPHOR: Palette = tokens([
    0x000000, 0x020403, 0x020403, 0x000000, 0x040805, 0x040805, 0x0d1a10, 0x1d3a24, 0x1d3a24,
    0x4ade80, 0x071209, 0x020503, 0x1d3a24, 0xc9f2d3, 0xecfff0, 0x5f8f6b, 0x7fbf8f, 0x22c55e,
    0x22c55e, 0x86efac, 0xfacc15, 0xf87171,
]);

const NEO_BRUTAL: Palette = tokens([
    0x0d0d0d, 0x0d0d0d, 0x111111, 0x0d0d0d, 0x161616, 0x161616, 0x262626, 0x4a4a4a, 0x4a4a4a,
    0xfafafa, 0x1c1c1c, 0x101010, 0x4a4a4a, 0xe5e5e5, 0xffffff, 0x9a9a9a, 0xbdbdbd, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const ROSE_NOIR: Palette = tokens([
    0x0b0809, 0x0d0a0b, 0x130e10, 0x0b0809, 0x191215, 0x140e11, 0x2a1c22, 0x3d2a32, 0x37252c,
    0xf9a8d4, 0x20161a, 0x110c0e, 0x3d2a32, 0xeddfe5, 0xfff7fb, 0xa3899a, 0xc4a8b6, 0xf472b6,
    0xf472b6, 0x86efac, 0xfcd34d, 0xfb7185,
]);

const SPOTLIGHT: Palette = tokens([
    0x030303, 0x050505, 0x070707, 0x030303, 0x121212, 0x0c0c0c, 0x232323, 0x2a2a2a, 0x4a4a4a,
    0xe5e5e5, 0x161616, 0x080808, 0x2a2a2a, 0xd6d6d6, 0xffffff, 0x828282, 0xa8a8a8, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

const TWO_TONE_FRAME: Palette = tokens([
    0x1b1b1f, 0x1b1b1f, 0x1b1b1f, 0x070708, 0x121215, 0x121215, 0x2a2a30, 0x2c2c33, 0x2c2c33,
    0x9a9aa4, 0x25252b, 0x18181c, 0x2c2c33, 0xd4d4d8, 0xfafafa, 0x85858f, 0xa6a6b0, 0x2dd4bf,
    0x2dd4bf, 0x4ade80, 0xfbbf24, 0xf87171,
]);

/// The palette of `style`; `dark` only matters for Neutral and Neon (every other style is dark-only).
pub(super) fn of(style: UiStyle, dark: bool) -> &'static Palette {
    match (style, dark) {
        (UiStyle::Neutral, true) => &NEUTRAL_DARK,
        (UiStyle::Neutral, false) => &NEUTRAL_LIGHT,
        (UiStyle::Neon, true) => &DARK,
        (UiStyle::Neon, false) => &LIGHT,
        (UiStyle::ZincContrast, _) => &ZINC_CONTRAST,
        (UiStyle::LayeredDepth, _) => &LAYERED_DEPTH,
        (UiStyle::TealSignal, _) => &TEAL_SIGNAL,
        (UiStyle::SlateInk, _) => &SLATE_INK,
        (UiStyle::MonoWire, _) => &MONO_WIRE,
        (UiStyle::GraphiteIslands, _) => &GRAPHITE_ISLANDS,
        (UiStyle::Aurora, _) => &AURORA,
        (UiStyle::ObsidianGlass, _) => &OBSIDIAN_GLASS,
        (UiStyle::Blueprint, _) => &BLUEPRINT,
        (UiStyle::Ember, _) => &EMBER,
        (UiStyle::IndigoNight, _) => &INDIGO_NIGHT,
        (UiStyle::Phosphor, _) => &PHOSPHOR,
        (UiStyle::NeoBrutal, _) => &NEO_BRUTAL,
        (UiStyle::RoseNoir, _) => &ROSE_NOIR,
        (UiStyle::Spotlight, _) => &SPOTLIGHT,
        (UiStyle::TwoToneFrame, _) => &TWO_TONE_FRAME,
    }
}
