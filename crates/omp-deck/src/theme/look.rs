//! Per-style structure ("look"): everything about a style that is not a colour token: region layout,
//! canvas decoration, card chrome, the subagent accordion, tab/selection styles and label fonts.
//! Values transcribe the `.tN …` rules of `docs/notes/theme-mockups.html`.

use deck_core::config::UiStyle;
use egui::Color32;

use super::palettes::rgb;

/// How the sidebar, main area and bars are separated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// Full-width hairline separators (sidebar | main, tab row, status bar).
    Lines,
    /// Sidebar and main are separate rounded, bordered panels on the window backdrop.
    Islands,
    /// Title bar, tabs, sidebar and status bar share one tone around a recessed canvas.
    Frame,
    /// 1 px separators that fade out at both ends.
    Fade,
}

/// Decoration painted behind non-terminal main content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Canvas {
    Flat,
    /// Dot grid (Blueprint).
    Dots,
    /// Horizontal scanlines (Phosphor).
    Scanlines,
    /// Violet and cyan radial glows at the top (Aurora).
    Aurora,
    /// White glow at the top, accent glow bottom-right (Obsidian Glass).
    Glass,
    /// Accent glow top-right (Rosé Noir).
    Rose,
    /// Soft white top glow plus an accent spotlight behind the active card (Spotlight).
    Spotlight,
    /// Accent wash fading out over `height` px from the top (Teal Signal, Ember).
    Wash {
        alpha: u8,
        height: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardShadow {
    None,
    /// Soft drop shadow below the card.
    Soft {
        dy: i8,
        blur: u8,
        alpha: u8,
    },
    /// Hard offset shadow (Neo-Brutal).
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    /// `hover_line` border and `hover_bg` fill.
    Ring,
    /// Border plus a second 1 px ring outside it.
    Ring2,
    /// Border plus ring plus a deeper shadow.
    Lift,
    /// Violet→cyan gradient border.
    Grad,
    /// Border plus a wide glow in the accent colour.
    Glow,
    /// Border plus a faint white halo and a deeper shadow.
    Halo,
    /// White border, the card shifts by (-1, -1) and casts a 6 px hard shadow.
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    /// Accent border.
    Border,
    /// Accent border and a `ring` px translucent accent ring.
    Ring { ring: u8, alpha: u8 },
    /// Violet→cyan gradient border, violet ring and glow.
    Grad,
    /// Accent border and a 2 px accent edge along the top.
    TopEdge,
    /// Accent border and a soft glow.
    Glow { blur: u8, alpha: u8 },
    /// Accent border and a large glow (Spotlight).
    Spot,
    /// Accent glow plus an inner ring (Phosphor).
    Phosphor,
    /// Accent border and a 5 px translucent accent hard shadow.
    Hard,
    /// Accent border and an accent-coloured second rule (Blueprint).
    Double,
}

/// Which accordion form the subagent section takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acc {
    /// Neutral / Neon: a plain row with a chevron.
    Row,
    /// Bordered box with a header band.
    Boxed,
    /// Edge-to-edge band at the card's foot.
    Band,
    /// Ruled table (no box): header with a dashed line, rows split by rules.
    Ruled,
    /// Dashed divider and mono header, outlined rows.
    Dashed,
    /// Pill-shaped header button with a nested panel for the rows.
    Pill,
    /// Fading divider above the header, floating row cards.
    Fade,
}

/// Fill of a header band, chevron chip or similar shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    None,
    /// `palette.raised`.
    Raised,
    /// The accent at this alpha.
    Accent(u8),
    /// Violet→cyan gradient.
    Grad,
    /// Opaque accent.
    AccentSolid,
    Fixed(Color32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChevInk {
    Weak,
    Accent,
    /// Near-black (on gradient / solid accent chips).
    Dark,
}

#[derive(Clone, Copy, Debug)]
pub struct Chev {
    pub round: bool,
    pub radius: u8,
    pub fill: Tone,
    /// Border colour: `None`, the palette stroke, or the accent.
    pub border: Border,
    pub ink: ChevInk,
    /// Inner top highlight.
    pub hi: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    None,
    Stroke,
    /// A brighter grey than the stroke (Mono Wire / Slate Ink outlines).
    Strong,
    Accent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rail {
    None,
    /// Accent line fading downwards (Teal Signal).
    Accent,
    /// Violet→cyan line (Aurora).
    Grad,
    /// Dashed `stroke` line (Phosphor).
    Dashed,
}

/// Look of one subagent row.
#[derive(Clone, Copy, Debug)]
pub struct Sub {
    /// Fill; `None` = transparent (rows separated by rules, or outlined only).
    pub fill: Option<Color32>,
    pub border: SubBorder,
    pub radius: u8,
    pub border_w: f32,
    /// Top-to-bottom gradient and inner highlight.
    pub glass: bool,
    pub shadow: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubBorder {
    /// Full outline in `stroke`.
    Box,
    /// Outline in the accordion's `tray_line`.
    Tray,
    /// Only a rule above the row (table / list).
    Rule,
}

#[derive(Clone, Copy, Debug)]
pub struct AccLook {
    pub kind: Acc,
    pub chev: Chev,
    /// Header height.
    pub h: f32,
    pub radius: u8,
    pub border_w: f32,
    /// Hard offset shadow of the box (Neo-Brutal).
    pub hard: bool,
    /// Inner top highlight of the box / header.
    pub hi: bool,
    pub header: Tone,
    /// Header text in `text_strong` instead of `text`.
    pub strong: bool,
    pub mono_header: bool,
    pub rail: Rail,
    pub segbar: bool,
    pub sub: Sub,
}

/// The style's touch on top of the slanted signal-edge tab (see `skin::paint_tab`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabKind {
    /// The plain signal edge: dark active plate, accent gradient outline, glow.
    Plain,
    /// Active tab filled with the sidebar tone (+ top highlight).
    Filled,
    /// Accent-tinted active plate.
    Tint,
    /// Inactive tabs outlined at full stroke strength.
    Outlined,
    /// Gradient outline (the signal edge itself is the look).
    Grad,
    /// Translucent glass plate with a top highlight.
    Glass,
    /// 2 px outlines, hard accent offset shadow on the active one.
    Brutal,
    /// Active tab filled with the canvas tone.
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sel {
    /// Raised fill with a 2 px accent bar.
    Bar,
    /// Accent tint and border.
    Tint,
    /// Raised fill with a violet→cyan bar.
    GradBar,
}

#[derive(Clone, Copy, Debug)]
pub struct Card {
    pub radius: u8,
    pub border_w: f32,
    /// Alpha of the 1 px inner top highlight (0 = none).
    pub hi: u8,
    pub shadow: CardShadow,
    /// A second outline 3 px outside every card (Blueprint).
    pub double_rule: bool,
    /// Opacity of cards that are neither active nor hovered.
    pub idle: f32,
    /// Header band over a dark body (Two-Tone Frame).
    pub two_tone: bool,
    pub hover: Hover,
    pub active: Active,
}

#[derive(Clone, Copy, Debug)]
pub struct Look {
    pub region: Region,
    /// Separator width.
    pub sep_w: f32,
    pub canvas: Canvas,
    /// Radius of rows, chips, fields and pills (flat styles).
    pub ui_r: u8,
    pub tab_r: u8,
    pub tab: TabKind,
    pub sel: Sel,
    pub card: Card,
    pub acc: AccLook,
    /// Labels, badges, meters, paths and ids in the monospace family.
    pub mono_labels: bool,
    /// The whole chrome in the monospace family.
    pub mono_ui: bool,
    /// Violet→cyan gradient on meters, the Agents chip and the "All" pill (Aurora).
    pub grad_accent: bool,
    /// Chips and badges are outlined and transparent.
    pub outlined_chips: bool,
    /// Fields and pills carry an inner top highlight.
    pub glass_fields: bool,
    /// Gap between the sidebar and the main area, and the margin around them (Islands).
    pub island_gap: u8,
    /// The sidebar casts a soft shadow onto the canvas (Layered Depth).
    pub cast: bool,
    /// An accent line under the tab row (Teal Signal).
    pub tab_rule: bool,
}

const NO_CHEV: Chev = Chev {
    round: false,
    radius: 5,
    fill: Tone::Raised,
    border: Border::None,
    ink: ChevInk::Weak,
    hi: false,
};

const BOX_SUB: Sub = Sub {
    fill: None,
    border: SubBorder::Box,
    radius: 6,
    border_w: 1.0,
    glass: false,
    shadow: false,
};

const FLAT_CARD: Card = Card {
    radius: 8,
    border_w: 1.0,
    hi: 0,
    shadow: CardShadow::None,
    double_rule: false,
    idle: 1.0,
    two_tone: false,
    hover: Hover::Ring,
    active: Active::Border,
};

const BASE_ACC: AccLook = AccLook {
    kind: Acc::Boxed,
    chev: NO_CHEV,
    h: 32.0,
    radius: 7,
    border_w: 1.0,
    hard: false,
    hi: false,
    header: Tone::None,
    strong: false,
    mono_header: false,
    rail: Rail::None,
    segbar: false,
    sub: BOX_SUB,
};

/// Neutral and Neon: the pre-gallery structure.
const BASE: Look = Look {
    region: Region::Lines,
    sep_w: 1.0,
    canvas: Canvas::Flat,
    ui_r: 8,
    tab_r: 6,
    tab: TabKind::Plain,
    sel: Sel::Bar,
    card: FLAT_CARD,
    acc: AccLook {
        kind: Acc::Row,
        h: 28.0,
        ..BASE_ACC
    },
    mono_labels: false,
    mono_ui: false,
    grad_accent: false,
    outlined_chips: false,
    glass_fields: false,
    island_gap: 6,
    cast: false,
    tab_rule: false,
};

pub(super) const fn look_of(style: UiStyle) -> Look {
    match style {
        UiStyle::Neutral | UiStyle::Neon => BASE,
        UiStyle::ZincContrast => Look {
            acc: AccLook {
                header: Tone::Fixed(rgb(0x1a1a1d)),
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::LayeredDepth => Look {
            cast: true,
            tab: TabKind::Filled,
            card: Card {
                radius: 10,
                hi: 18,
                shadow: CardShadow::Soft {
                    dy: 10,
                    blur: 28,
                    alpha: 140,
                },
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Band,
                h: 36.0,
                chev: Chev {
                    hi: true,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x19191d)),
                    glass: true,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::TealSignal => Look {
            tab_rule: true,
            tab: TabKind::Tint,
            sel: Sel::Tint,
            canvas: Canvas::Wash {
                alpha: 18,
                height: 260,
            },
            card: Card {
                active: Active::Ring { ring: 3, alpha: 31 },
                ..FLAT_CARD
            },
            acc: AccLook {
                strong: true,
                rail: Rail::Accent,
                chev: Chev {
                    fill: Tone::Accent(36),
                    ink: ChevInk::Accent,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x101414)),
                    border: SubBorder::Box,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::SlateInk => Look {
            card: Card {
                radius: 10,
                hi: 10,
                shadow: CardShadow::Soft {
                    dy: 4,
                    blur: 14,
                    alpha: 90,
                },
                hover: Hover::Ring2,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Band,
                h: 36.0,
                chev: Chev {
                    fill: Tone::None,
                    border: Border::Stroke,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: None,
                    border: SubBorder::Rule,
                    radius: 0,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::MonoWire => Look {
            tab: TabKind::Outlined,
            outlined_chips: true,
            card: Card {
                radius: 6,
                hover: Hover::Ring2,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Dashed,
                h: 30.0,
                mono_header: true,
                strong: true,
                chev: Chev {
                    fill: Tone::None,
                    border: Border::Strong,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x0f0f0f)),
                    border: SubBorder::Box,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::GraphiteIslands => Look {
            region: Region::Islands,
            tab: TabKind::Filled,
            card: Card {
                radius: 12,
                hi: 20,
                shadow: CardShadow::Soft {
                    dy: 12,
                    blur: 30,
                    alpha: 150,
                },
                hover: Hover::Lift,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Pill,
                h: 30.0,
                hi: true,
                header: Tone::Raised,
                chev: Chev {
                    round: true,
                    radius: 10,
                    fill: Tone::Fixed(rgb(0x303036)),
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x1a1a1e)),
                    radius: 8,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::Aurora => Look {
            tab: TabKind::Grad,
            sel: Sel::GradBar,
            canvas: Canvas::Aurora,
            grad_accent: true,
            card: Card {
                radius: 12,
                hi: 13,
                shadow: CardShadow::Soft {
                    dy: 10,
                    blur: 30,
                    alpha: 140,
                },
                hover: Hover::Grad,
                active: Active::Grad,
                ..FLAT_CARD
            },
            acc: AccLook {
                radius: 10,
                rail: Rail::Grad,
                header: Tone::Grad,
                chev: Chev {
                    round: true,
                    radius: 10,
                    fill: Tone::Grad,
                    ink: ChevInk::Dark,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x0f0f18)),
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::ObsidianGlass => Look {
            tab: TabKind::Glass,
            tab_r: 9,
            glass_fields: true,
            canvas: Canvas::Glass,
            card: Card {
                radius: 16,
                hi: 36,
                shadow: CardShadow::Soft {
                    dy: 20,
                    blur: 50,
                    alpha: 150,
                },
                hover: Hover::Halo,
                active: Active::Glow {
                    blur: 30,
                    alpha: 31,
                },
                ..FLAT_CARD
            },
            acc: AccLook {
                radius: 12,
                hi: true,
                chev: Chev {
                    round: true,
                    radius: 10,
                    fill: Tone::Fixed(rgb(0x1a1a1a)),
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x0a0a0a)),
                    radius: 10,
                    glass: true,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::Blueprint => Look {
            canvas: Canvas::Dots,
            ui_r: 3,
            tab_r: 3,
            mono_labels: true,
            card: Card {
                radius: 3,
                double_rule: true,
                active: Active::Double,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Ruled,
                h: 30.0,
                mono_header: true,
                strong: true,
                chev: Chev {
                    radius: 2,
                    fill: Tone::None,
                    border: Border::Accent,
                    ink: ChevInk::Accent,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: None,
                    border: SubBorder::Rule,
                    radius: 0,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::Ember => Look {
            canvas: Canvas::Wash {
                alpha: 15,
                height: 220,
            },
            card: Card {
                radius: 10,
                hi: 13,
                shadow: CardShadow::Soft {
                    dy: 8,
                    blur: 24,
                    alpha: 130,
                },
                active: Active::TopEdge,
                ..FLAT_CARD
            },
            acc: AccLook {
                radius: 8,
                strong: true,
                header: Tone::Accent(30),
                chev: Chev {
                    fill: Tone::Accent(50),
                    ink: ChevInk::Accent,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x18130f)),
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::IndigoNight => Look {
            card: Card {
                radius: 12,
                hi: 15,
                shadow: CardShadow::Soft {
                    dy: 10,
                    blur: 30,
                    alpha: 130,
                },
                hover: Hover::Glow,
                active: Active::Ring { ring: 3, alpha: 36 },
                ..FLAT_CARD
            },
            acc: AccLook {
                radius: 10,
                segbar: true,
                chev: Chev {
                    radius: 6,
                    fill: Tone::Accent(46),
                    ink: ChevInk::Accent,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x10102b)),
                    border: SubBorder::Tray,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::Phosphor => Look {
            canvas: Canvas::Scanlines,
            ui_r: 2,
            tab_r: 2,
            mono_labels: true,
            mono_ui: true,
            outlined_chips: true,
            card: Card {
                radius: 2,
                active: Active::Phosphor,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Dashed,
                h: 30.0,
                rail: Rail::Dashed,
                chev: Chev {
                    radius: 2,
                    fill: Tone::None,
                    border: Border::Stroke,
                    ink: ChevInk::Accent,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: None,
                    border: SubBorder::Box,
                    radius: 2,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::NeoBrutal => Look {
            sep_w: 2.0,
            tab: TabKind::Brutal,
            card: Card {
                radius: 6,
                border_w: 2.0,
                shadow: CardShadow::Hard,
                hover: Hover::Hard,
                active: Active::Hard,
                ..FLAT_CARD
            },
            acc: AccLook {
                radius: 5,
                border_w: 2.0,
                hard: true,
                strong: true,
                header: Tone::Fixed(rgb(0x222222)),
                chev: Chev {
                    radius: 3,
                    fill: Tone::AccentSolid,
                    ink: ChevInk::Dark,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x141414)),
                    border_w: 2.0,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::RoseNoir => Look {
            tab_r: 14,
            canvas: Canvas::Rose,
            card: Card {
                radius: 14,
                hi: 13,
                shadow: CardShadow::Soft {
                    dy: 12,
                    blur: 32,
                    alpha: 130,
                },
                hover: Hover::Halo,
                active: Active::Ring { ring: 4, alpha: 31 },
                ..FLAT_CARD
            },
            acc: AccLook {
                radius: 12,
                strong: true,
                header: Tone::Accent(20),
                chev: Chev {
                    round: true,
                    radius: 10,
                    fill: Tone::Accent(56),
                    ink: ChevInk::Accent,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x1a1317)),
                    radius: 10,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::Spotlight => Look {
            region: Region::Fade,
            canvas: Canvas::Spotlight,
            card: Card {
                radius: 12,
                hi: 23,
                shadow: CardShadow::Soft {
                    dy: 18,
                    blur: 40,
                    alpha: 200,
                },
                idle: 0.82,
                hover: Hover::Halo,
                active: Active::Spot,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Fade,
                h: 38.0,
                strong: true,
                chev: Chev {
                    round: true,
                    radius: 10,
                    fill: Tone::Fixed(rgb(0x1e1e1e)),
                    hi: true,
                    ..NO_CHEV
                },
                sub: Sub {
                    fill: Some(rgb(0x161616)),
                    border: SubBorder::Box,
                    radius: 10,
                    glass: true,
                    shadow: true,
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
        UiStyle::TwoToneFrame => Look {
            region: Region::Frame,
            tab: TabKind::Folder,
            card: Card {
                radius: 10,
                two_tone: true,
                ..FLAT_CARD
            },
            acc: AccLook {
                kind: Acc::Band,
                h: 36.0,
                strong: true,
                sub: Sub {
                    fill: Some(rgb(0x121215)),
                    ..BOX_SUB
                },
                ..BASE_ACC
            },
            ..BASE
        },
    }
}
