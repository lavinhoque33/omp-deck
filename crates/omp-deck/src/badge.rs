//! Tinted value badges: one fixed hue per meaning (cost green, time amber, requests blue, tools
//! orange, tokens teal, model violet, thinking rose), so numbers that used to be one grey run of
//! text read as separate facts. Hues are fixed rather than taken from the style's accent: most
//! styles have a single accent, and two neighbouring badges in the same colour would blur again.
//!
//! A [`Badge`] is laid out once (galleys measured) and then painted either at a point (cards,
//! tree rows) or allocated in a `Ui` (tooltips, the status bar).

use std::sync::Arc;

use egui::{
    Align2, Color32, Galley, Painter, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::icons::Icon;
use crate::theme::{self, Palette};
use crate::widgets::paint_icon;

/// What a badge stands for; see the module docs for the assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    Blue,
    Green,
    Amber,
    Violet,
    Teal,
    Rose,
    Orange,
}

impl Hue {
    /// The hue's text/outline colour: bright on dark themes, deep on light ones.
    pub fn color(self, pal: &Palette) -> Color32 {
        let dark = pal.bg.r() < 128;
        let (d, l) = match self {
            Hue::Blue => ((96, 165, 250), (37, 99, 235)),
            Hue::Green => ((74, 222, 128), (22, 163, 74)),
            Hue::Amber => ((251, 191, 36), (180, 110, 4)),
            Hue::Violet => ((192, 132, 252), (147, 51, 234)),
            Hue::Teal => ((45, 212, 191), (13, 148, 136)),
            Hue::Rose => ((244, 114, 182), (219, 39, 119)),
            Hue::Orange => ((251, 146, 60), (234, 88, 12)),
        };
        let (r, g, b) = if dark { d } else { l };
        Color32::from_rgb(r, g, b)
    }
}

const PAD_X: f32 = 7.0;
const PART_GAP: f32 = 5.0;
const ICON_GAP: f32 = 4.0;

/// A laid-out badge: optional icon, one or more text parts (each its own colour), one tint.
pub struct Badge {
    icon: Option<(Icon, f32)>,
    parts: Vec<(Arc<Galley>, Color32)>,
    tint: Color32,
    w: f32,
    h: f32,
}

impl Badge {
    /// `parts` are `(text, colour)` runs drawn left to right inside one outline tinted `tint`;
    /// `size` is the text size, the badge is `size + 7` tall.
    pub fn new(
        p: &Painter,
        icon: Option<Icon>,
        parts: &[(&str, Color32)],
        tint: Color32,
        size: f32,
    ) -> Badge {
        let parts: Vec<(Arc<Galley>, Color32)> = parts
            .iter()
            .map(|(t, c)| (p.layout_no_wrap((*t).to_owned(), theme::font(size), *c), *c))
            .collect();
        let icon = icon.map(|i| (i, size + 1.5));
        let mut w = PAD_X * 2.0;
        if let Some((_, s)) = icon {
            w += s + ICON_GAP;
        }
        w += parts.iter().map(|(g, _)| g.size().x).sum::<f32>();
        w += PART_GAP * parts.len().saturating_sub(1) as f32;
        Badge {
            icon,
            parts,
            tint,
            w: w.ceil(),
            h: (size + 7.0).round(),
        }
    }

    /// One text run in the badge's own tint.
    pub fn single(p: &Painter, icon: Option<Icon>, text: &str, tint: Color32, size: f32) -> Badge {
        Badge::new(p, icon, &[(text, tint)], tint, size)
    }

    pub fn width(&self) -> f32 {
        self.w
    }

    /// Paints with the left edge at `left`, vertically centred on `cy`; returns its rect.
    pub fn paint_at(&self, p: &Painter, left: f32, cy: f32) -> Rect {
        let rect = Rect::from_min_size(pos2(left, cy - self.h / 2.0), vec2(self.w, self.h));
        self.paint_in(p, rect);
        rect
    }

    fn paint_in(&self, p: &Painter, rect: Rect) {
        let r = theme::ui_rad((self.h / 2.0) as u8);
        p.rect_filled(rect, r, self.tint.gamma_multiply(0.16));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, self.tint.gamma_multiply(0.45)),
            StrokeKind::Inside,
        );
        let cy = rect.center().y;
        let mut x = rect.left() + PAD_X;
        if let Some((icon, s)) = self.icon {
            paint_icon(
                p,
                pos2(x + s / 2.0, cy),
                Align2::CENTER_CENTER,
                icon,
                s,
                self.tint,
            );
            x += s + ICON_GAP;
        }
        for (g, c) in &self.parts {
            p.galley(pos2(x, cy - g.size().y / 2.0), Arc::clone(g), *c);
            x += g.size().x + PART_GAP;
        }
    }

    /// Allocates the badge in `ui` and paints it.
    pub fn show(&self, ui: &mut Ui) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(self.w, self.h), Sense::hover());
        if ui.is_rect_visible(rect) {
            self.paint_in(ui.painter(), rect);
        }
        resp
    }
}

/// Shorthand: a single-run badge allocated in `ui`.
pub fn show(ui: &mut Ui, icon: Option<Icon>, text: &str, hue: Hue, size: f32) -> Response {
    let pal = Palette::of_ui(ui);
    Badge::single(ui.painter(), icon, text, hue.color(pal), size).show(ui)
}
