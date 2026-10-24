//! The subagent accordion of the agents dashboard in each style: geometry, backdrop, chevron
//! chip, rails, the status segment bar and the look of the subagent rows.

use egui::{
    Align2, Color32, CornerRadius, Id, Painter, Pos2, Rect, Stroke, StrokeKind, pos2, vec2,
};

use super::skin::{self, Dir, alpha, black, white};
use super::{Acc, Border, ChevInk, Palette, Rail, SubBorder, Tone, look, mix};
use crate::icons::{ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT};
use crate::widgets::paint_icon;

/// Distances of the accordion inside a card, from `pad` (the card's inner padding).
/// Horizontal values are measured from the card's outer edges.
#[derive(Clone, Copy, Debug)]
pub struct Geo {
    /// Header height.
    pub h: f32,
    /// Spans the card edge to edge and ends at the card's bottom edge.
    pub edge: bool,
    /// Left and right inset of the rows.
    pub rows_l: f32,
    pub rows_r: f32,
    /// Space between the header and the rows, and below the rows.
    pub pad_top: f32,
    pub pad_bottom: f32,
    /// Space between the accordion and the card's bottom edge.
    pub foot: f32,
    /// Left inset of the accordion's own box / header.
    pub box_l: f32,
    pub box_r: f32,
}

pub fn geo(pad: f32) -> Geo {
    let lk = look();
    let a = lk.acc;
    let rule_rows = a.sub.border == SubBorder::Rule;
    match a.kind {
        Acc::Row => Geo {
            h: 28.0,
            edge: false,
            rows_l: pad,
            rows_r: pad,
            pad_top: 6.0,
            pad_bottom: pad,
            foot: pad,
            box_l: pad,
            box_r: pad,
        },
        Acc::Boxed => Geo {
            h: a.h,
            edge: false,
            // A rail hangs left of the rows: 22 px of padding plus the box border.
            rows_l: pad + if a.rail == Rail::None { 9.0 } else { 23.0 },
            rows_r: pad + 9.0,
            pad_top: 9.0,
            pad_bottom: 3.0,
            foot: pad,
            box_l: pad,
            box_r: pad,
        },
        Acc::Band => Geo {
            h: a.h,
            edge: true,
            rows_l: if rule_rows { 0.0 } else { 16.0 },
            rows_r: if rule_rows { 0.0 } else { 12.0 },
            pad_top: 2.0,
            pad_bottom: if rule_rows { 4.0 } else { 6.0 },
            foot: 0.0,
            box_l: 0.0,
            box_r: 0.0,
        },
        Acc::Ruled => Geo {
            h: a.h,
            edge: false,
            rows_l: pad + 1.0,
            rows_r: pad + 1.0,
            pad_top: 0.0,
            pad_bottom: 1.0,
            foot: pad,
            box_l: pad,
            box_r: pad,
        },
        Acc::Dashed => Geo {
            h: a.h,
            edge: false,
            rows_l: pad + if a.rail == Rail::Dashed { 27.0 } else { 0.0 },
            rows_r: pad,
            pad_top: 4.0,
            pad_bottom: 0.0,
            foot: pad,
            box_l: pad,
            box_r: pad,
        },
        Acc::Pill => Geo {
            h: a.h,
            edge: false,
            rows_l: pad + 28.0,
            rows_r: pad + 9.0,
            pad_top: 17.0,
            pad_bottom: 3.0,
            foot: pad,
            box_l: pad,
            box_r: pad,
        },
        Acc::Fade => Geo {
            h: a.h,
            edge: false,
            rows_l: pad,
            rows_r: pad,
            pad_top: 2.0,
            pad_bottom: 6.0,
            foot: pad,
            box_l: pad,
            box_r: pad,
        },
    }
}

/// Where the header's pieces go, returned by [`paint_back`].
#[derive(Clone, Copy, Debug)]
pub struct Slots {
    /// Left edge of the first content item after the chevron.
    pub text_x: f32,
    /// Right edge available to the header's right-hand hint.
    pub right: f32,
}

/// Everything [`paint_back`] needs to know about one accordion.
#[derive(Clone, Copy)]
pub struct Frame {
    pub card: Rect,
    /// Header row (full box width).
    pub header: Rect,
    /// Bottom of the accordion panel (`header.bottom()` when closed).
    pub bottom: f32,
    pub open: bool,
    pub hovered: bool,
    pub card_hovered: bool,
    /// Width of the header pill (Pill style); other styles ignore it.
    pub pill_w: f32,
}

/// Paints the accordion's box / band / header and its chevron chip.
pub fn paint_back(painter: &Painter, id: Id, pal: &Palette, f: &Frame) -> Slots {
    let lk = look();
    let a = lk.acc;
    let h = f.header;
    let cy = h.center().y;
    let r = f32::from(a.radius);
    let cr = CornerRadius::same(a.radius);
    let hi_alpha = 15;
    let full = Rect::from_min_max(h.min, pos2(h.right(), f.bottom));
    let hover_lift = |rect: Rect, rounding: CornerRadius| {
        if f.hovered {
            painter.rect_filled(rect, rounding, white(8));
        }
    };
    let stroke = Stroke::new(a.border_w, pal.tray_line);
    let top_cr = CornerRadius {
        nw: a.radius.saturating_sub(1),
        ne: a.radius.saturating_sub(1),
        sw: 0,
        se: 0,
    };
    let header_fill = |rect: Rect, rounding: CornerRadius| match a.header {
        Tone::None => {}
        Tone::Raised => {
            painter.rect_filled(rect, rounding, pal.raised);
        }
        Tone::Fixed(c) => {
            let c = if f.card_hovered {
                mix(c, white(255), 0.06)
            } else {
                c
            };
            painter.rect_filled(rect, rounding, c);
        }
        Tone::Accent(al) => {
            painter.rect_filled(rect, rounding, alpha(pal.accent, al));
        }
        Tone::Grad => {
            skin::gradient_corners(
                painter,
                id.with("hf"),
                rect,
                [
                    f32::from(rounding.nw),
                    f32::from(rounding.ne),
                    f32::from(rounding.se),
                    f32::from(rounding.sw),
                ],
                (alpha(pal.accent_a, 31), alpha(pal.accent_b, 10)),
                Dir::Right,
            );
        }
        Tone::AccentSolid => {
            painter.rect_filled(rect, rounding, pal.accent);
        }
    };
    let (chev_x, text_x, right) = match a.kind {
        Acc::Row => {
            return Slots {
                text_x: h.left() + 26.0,
                right: h.right(),
            };
        }
        Acc::Boxed => {
            // Hard shadow, box, header band, divider.
            if a.hard {
                painter.rect_filled(full.translate(vec2(3.0, 3.0)), cr, black(255));
            }
            painter.rect_filled(full, cr, pal.tray);
            if f.open {
                let band = Rect::from_min_size(h.min, vec2(h.width(), a.h));
                header_fill(band.shrink(a.border_w), top_cr);
            } else {
                header_fill(h.shrink(a.border_w), cr);
            }
            if a.hi {
                skin::top_highlight(painter, full, r, hi_alpha);
            }
            hover_lift(h.shrink(a.border_w), if f.open { top_cr } else { cr });
            painter.rect_stroke(full, cr, stroke, StrokeKind::Inside);
            if f.open {
                painter.hline(
                    full.x_range(),
                    h.top() + a.h - a.border_w / 2.0,
                    Stroke::new(a.border_w, pal.tray_line),
                );
            }
            (h.left() + 20.0, h.left() + 38.0, h.right() - 10.0)
        }
        Acc::Band => {
            let bottom_cr = CornerRadius {
                nw: 0,
                ne: 0,
                sw: lk.card.radius.saturating_sub(1),
                se: lk.card.radius.saturating_sub(1),
            };
            let band = Rect::from_min_max(
                pos2(f.card.left() + 1.0, h.top()),
                pos2(f.card.right() - 1.0, f.bottom - 1.0),
            );
            painter.rect_filled(band, bottom_cr, pal.tray);
            painter.hline(
                f.card.x_range(),
                h.top() + 0.5,
                Stroke::new(1.0, pal.tray_line),
            );
            if lk.card.hi == 18 || a.hi {
                // Sunken: shadow under the top rule.
                painter.rect_filled(
                    Rect::from_min_size(pos2(band.left(), h.top() + 1.0), vec2(band.width(), 4.0)),
                    0,
                    black(60),
                );
            }
            if f.open && lk.card.two_tone {
                painter.hline(
                    band.x_range(),
                    h.top() + a.h - 0.5,
                    Stroke::new(1.0, pal.stroke),
                );
            }
            hover_lift(
                Rect::from_min_size(band.min, vec2(band.width(), a.h - 1.0)),
                CornerRadius::ZERO,
            );
            (
                f.card.left() + 26.0,
                f.card.left() + 44.0,
                f.card.right() - 14.0,
            )
        }
        Acc::Ruled => {
            // No box: the header, then (open) a ruled table around the rows.
            if f.open {
                let table =
                    Rect::from_min_max(pos2(h.left(), h.bottom()), pos2(h.right(), f.bottom));
                painter.rect_stroke(table, 0, Stroke::new(1.0, pal.stroke), StrokeKind::Inside);
            }
            (h.left() + 10.0, h.left() + 28.0, h.right())
        }
        Acc::Dashed => {
            let col = if lk.mono_ui {
                pal.stroke
            } else {
                rgb(0x3a3a3a)
            };
            skin::dashed_hline(painter, h.left(), h.right(), h.top() + 0.5, col);
            (h.left() + 12.0, h.left() + 30.0, h.right() - 2.0)
        }
        Acc::Pill => {
            let pill = Rect::from_min_size(h.min, vec2(f.pill_w.min(h.width()), a.h));
            let pr = a.h / 2.0;
            painter.rect_filled(pill, pr as u8, pal.raised);
            painter.rect_stroke(
                pill,
                pr as u8,
                Stroke::new(1.0, pal.stroke),
                StrokeKind::Inside,
            );
            skin::top_highlight(painter, pill, pr, 18);
            if f.hovered {
                painter.rect_stroke(
                    pill,
                    pr as u8,
                    Stroke::new(1.0, pal.hover_line),
                    StrokeKind::Inside,
                );
            }
            if f.open {
                // Nested panel below, joined by a connector.
                let panel = Rect::from_min_max(
                    pos2(h.left() + 14.0, h.bottom() + 8.0),
                    pos2(h.right(), f.bottom),
                );
                painter.rect_filled(panel, 10, pal.tray);
                painter.rect_stroke(
                    panel,
                    10,
                    Stroke::new(1.0, pal.tray_line),
                    StrokeKind::Inside,
                );
                painter.vline(
                    panel.left() + 0.5,
                    (panel.top() - 8.0)..=panel.top(),
                    Stroke::new(1.0, pal.stroke),
                );
            }
            (pill.left() + 16.0, pill.left() + 34.0, pill.right() - 12.0)
        }
        Acc::Fade => {
            skin::fade_rule(painter, id.with("fr"), h.left(), h.right(), h.top() + 0.5);
            (h.left() + 10.0, h.left() + 28.0, h.right() - 2.0)
        }
    };
    let chev = pos2(
        chev_x,
        cy + if matches!(a.kind, Acc::Dashed | Acc::Fade) {
            1.0
        } else {
            0.0
        },
    );
    paint_chev(painter, id, pal, chev, f.open, f.hovered);
    Slots { text_x, right }
}

/// Dashed fill from `x` to the header's right edge (Ruled style), drawn after the content.
pub fn paint_dashed_fill(painter: &Painter, pal: &Palette, header: Rect, x: f32, right: f32) {
    if look().acc.kind == Acc::Ruled && right > x {
        skin::dashed_hline(painter, x, right, header.center().y, pal.stroke);
    }
}

/// The chevron chip.
pub fn paint_chev(painter: &Painter, id: Id, pal: &Palette, c: Pos2, open: bool, hovered: bool) {
    let chev = look().acc.chev;
    let size = if look().acc.kind == Acc::Row {
        15.0
    } else {
        20.0
    };
    let rect = Rect::from_center_size(c, vec2(size, size));
    if look().acc.kind != Acc::Row {
        let cr = if chev.round {
            CornerRadius::same(10)
        } else {
            CornerRadius::same(chev.radius)
        };
        match chev.fill {
            Tone::None => {}
            Tone::Raised => {
                painter.rect_filled(rect, cr, pal.raised);
            }
            Tone::Accent(al) => {
                painter.rect_filled(rect, cr, alpha(pal.accent, al));
            }
            Tone::AccentSolid => {
                painter.rect_filled(rect, cr, pal.accent);
            }
            Tone::Fixed(col) => {
                painter.rect_filled(rect, cr, col);
            }
            Tone::Grad => skin::gradient_card(
                painter,
                id.with("chev"),
                rect,
                10.0,
                (pal.accent_a, pal.accent_b),
                Dir::Diagonal,
            ),
        }
        if chev.hi {
            skin::top_highlight(painter, rect, 8.0, 15);
        }
        let border = match chev.border {
            Border::None => None,
            Border::Stroke => Some(pal.stroke),
            Border::Strong => Some(rgb(0x4a4a4a)),
            Border::Accent => Some(pal.accent),
        };
        if let Some(b) = border {
            painter.rect_stroke(rect, cr, Stroke::new(1.0, b), StrokeKind::Inside);
        }
    }
    let ink = match chev.ink {
        ChevInk::Weak => {
            if hovered {
                pal.text_strong
            } else {
                pal.weak
            }
        }
        ChevInk::Accent => pal.accent,
        ChevInk::Dark => rgb(0x0b0b12),
    };
    paint_icon(
        painter,
        c,
        Align2::CENTER_CENTER,
        if open {
            ICON_CHEVRON_DOWN
        } else {
            ICON_CHEVRON_RIGHT
        },
        if look().acc.kind == Acc::Row {
            15.0
        } else {
            14.0
        },
        ink,
    );
}

fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The vertical rail the rows hang off (Teal Signal, Aurora, Phosphor).
pub fn paint_rail(painter: &Painter, id: Id, pal: &Palette, x: f32, y0: f32, y1: f32) {
    if y1 <= y0 {
        return;
    }
    match look().acc.rail {
        Rail::None => {}
        Rail::Accent => {
            let r = Rect::from_min_max(pos2(x - 1.0, y0), pos2(x + 1.0, y1));
            // Fades from the accent to a faint tail: two stacked segments.
            let mid = y0 + (y1 - y0) * 0.5;
            let top = Rect::from_min_max(r.min, pos2(r.max.x, mid));
            let bottom = Rect::from_min_max(pos2(r.min.x, mid), r.max);
            fade_v(
                painter,
                id.with("r0"),
                top,
                alpha(pal.accent, 255),
                alpha(pal.accent, 140),
            );
            fade_v(
                painter,
                id.with("r1"),
                bottom,
                alpha(pal.accent, 140),
                alpha(pal.accent, 38),
            );
        }
        Rail::Grad => {
            let r = Rect::from_min_max(pos2(x - 1.0, y0), pos2(x + 1.0, y1));
            fade_v(painter, id.with("r0"), r, pal.accent_a, pal.accent_b);
        }
        Rail::Dashed => skin::dashed_vline(painter, x, y0, y1, pal.stroke),
    }
}

fn fade_v(painter: &Painter, id: Id, rect: Rect, a: Color32, b: Color32) {
    skin::gradient_card(painter, id, rect, 0.0, (a, b), Dir::Down);
}

/// Segment bar (Indigo Night): one segment per subagent in status order, merged proportionally
/// beyond [`MAX_SEGMENTS`] so each stays readable. `segs` = `(count, colour)` per status group.
pub fn paint_segbar(painter: &Painter, rect: Rect, segs: &[(usize, Color32)]) {
    let total: usize = segs.iter().map(|s| s.0).sum();
    if total == 0 {
        return;
    }
    let n = total.min(MAX_SEGMENTS);
    let gap = 2.0;
    let w = (rect.width() - gap * (n as f32 - 1.0)) / n as f32;
    for k in 0..n {
        // The agent this segment stands for, and the group that agent belongs to.
        let agent = k * total / n;
        let mut seen = 0;
        let Some((_, col)) = segs.iter().find(|(count, _)| {
            seen += count;
            agent < seen
        }) else {
            continue;
        };
        let x = rect.left() + k as f32 * (w + gap);
        painter.rect_filled(
            Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height())),
            2,
            *col,
        );
    }
}

/// Most segments the Indigo status bar shows.
pub const MAX_SEGMENTS: usize = 10;

// ------------------------------------------------------------------ rows

/// Paints a subagent row's backdrop. `rect` is the row card; `tint` is the legacy status wash
/// used by Neutral and Neon only.
pub fn paint_sub(
    painter: &Painter,
    id: Id,
    pal: &Palette,
    rect: Rect,
    hovered: bool,
    selected: bool,
    legacy_fill: Color32,
) {
    let lk = look();
    let s = lk.acc.sub;
    if lk.acc.kind == Acc::Row {
        return paint_legacy(painter, pal, rect, hovered, selected, legacy_fill);
    }
    let r = f32::from(s.radius);
    let cr = CornerRadius::same(s.radius);
    let base = s.fill;
    if let Some(fill) = base {
        let fill = if hovered {
            mix(fill, pal.hover_bg, 0.6)
        } else {
            fill
        };
        if s.glass {
            skin::gradient_card(
                painter,
                id.with("sg"),
                rect,
                r,
                (mix(fill, white(255), 0.03), fill),
                Dir::Down,
            );
        } else {
            painter.rect_filled(rect, cr, fill);
        }
        if s.shadow {
            painter.add(
                egui::Shadow {
                    offset: [0, 6],
                    blur: 14,
                    spread: 0,
                    color: black(150),
                }
                .as_shape(rect, cr),
            );
        }
    } else if hovered {
        painter.rect_filled(rect, cr, pal.hover_bg);
    }
    let line = if selected {
        pal.accent
    } else if hovered {
        // Brighter than the hover line of cards: rows are small and sit on a darker tray.
        mix(pal.hover_line, pal.text_strong, 0.4)
    } else if s.border == SubBorder::Tray {
        rgb(0x26265a)
    } else {
        pal.stroke
    };
    match s.border {
        SubBorder::Box | SubBorder::Tray => {
            let w = if hovered {
                s.border_w + 0.5
            } else {
                s.border_w
            };
            painter.rect_stroke(rect, cr, Stroke::new(w, line), StrokeKind::Inside);
        }
        SubBorder::Rule => {
            let edge = if selected || hovered {
                line
            } else {
                pal.tray_line
            };
            painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, edge));
            if hovered {
                painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, edge));
            }
        }
    }
    if s.glass {
        skin::top_highlight(painter, rect, r, 14);
    }
}

fn paint_legacy(
    painter: &Painter,
    pal: &Palette,
    rect: Rect,
    hovered: bool,
    selected: bool,
    fill: Color32,
) {
    let r = if super::flat() { 8 } else { 11 };
    painter.rect_filled(rect, r, fill);
    let border = if selected {
        pal.accent
    } else if hovered {
        super::hover_stroke(pal)
    } else {
        pal.stroke
    };
    painter.rect_stroke(rect, r, Stroke::new(1.0, border), StrokeKind::Inside);
}

/// Radius of the subagent rows' status stripe, and whether rows are separated by rules.
pub fn rows_use_rules() -> bool {
    look().acc.sub.border == SubBorder::Rule
}

/// Gap between subagent rows.
pub fn row_gap() -> f32 {
    if rows_use_rules() { 0.0 } else { 6.0 }
}
