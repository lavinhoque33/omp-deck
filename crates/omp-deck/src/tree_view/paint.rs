//! Turn-trail card and graph painting in the chat view's visual language: outlined cards
//! with `0 10 0 10` corners, accent/Violet/warm washes by row class, ringed node markers, and the
//! git-graph edges (active path in accent, thicker; alternates muted per lane).

use egui::Painter;
use egui::epaint::CubicBezierShape;
use egui::{Color32, Pos2, Stroke, pos2};

use crate::chat_view::Ink;
use crate::chat_view::paint as chat_paint;
use crate::deco;
use crate::icons::Icon;
use crate::icons::{
    ICON_ALIGN_LEFT, ICON_FOLD_VERTICAL, ICON_SCROLL_TEXT, ICON_SPARKLES, ICON_USER, ICON_WRENCH,
};
use crate::theme::{self, Palette};
use crate::tree_layout::{Layout, Row, RowClass};
use crate::tree_view::RowUi;
use crate::tree_view::geometry::{CARD_GAP, Geometry, row_height};

/// An [`Ink`] for the tree window: the chat view's colours at the tree's mono size.
pub fn ink_of(ui: &egui::Ui, mono_size: f32) -> Ink {
    Ink::new(ui, mono_size)
}

/// Role colour of a row class: user prompts read in the accent, like omp 18.16.
pub fn class_color(ink: &Ink, class: RowClass) -> Color32 {
    let pal = ink.p;
    match class {
        RowClass::User if theme::flat() => pal.text_strong,
        RowClass::Assistant if theme::flat() => pal.text,
        RowClass::User => ink.a,
        RowClass::Assistant => ink.a.gamma_multiply(0.9),
        RowClass::Tool => pal.weak,
        RowClass::Compaction => pal.warn,
        RowClass::Summary => ink.b,
        RowClass::Other => pal.weak,
    }
}

pub fn class_icon(class: RowClass) -> Icon {
    match class {
        RowClass::User => ICON_USER,
        RowClass::Assistant => ICON_SPARKLES,
        RowClass::Tool => ICON_WRENCH,
        RowClass::Compaction => ICON_FOLD_VERTICAL,
        RowClass::Summary => ICON_SCROLL_TEXT,
        RowClass::Other => ICON_ALIGN_LEFT,
    }
}

/// Muted tint of an inactive branch, from its lane.
pub fn lane_color(ink: &Ink, lane: u16) -> Color32 {
    let pal = ink.p;
    let hues = [ink.b, pal.warn, pal.ok, pal.danger, ink.a];
    theme::mix(pal.weak, hues[usize::from(lane) % hues.len()], 0.38).gamma_multiply(0.85)
}

/// The card of one row: outlined `0 10 0 10` card right of the gutter; `hov_t` is the animated
/// hover strength 0..=1.
pub fn paint_card(
    p: &Painter,
    ink: &Ink,
    layout: &Layout,
    geo: &Geometry<'_>,
    u: &RowUi,
    selected: bool,
    hov_t: f32,
) {
    let row = &layout.rows[u.row];
    let card = geo.card(u.row, row.class);
    let off_path = !row.on_path;
    let tool = row.class == RowClass::Tool;
    let pts = chat_paint::corners_rect(card, 0.0, 10.0, 0.0, 10.0);

    let (fill, edge) = card_look(ink, row, selected);
    if selected && !theme::flat() {
        let (a, b) = ink.head();
        let (a, b) = (theme::mix(a, ink.a, 0.25), theme::mix(b, ink.b, 0.25));
        chat_paint::fill_h(p, &pts, a, b);
    } else {
        deco::fill(p, pts.clone(), fill);
    }
    if !theme::flat() && !tool {
        p.add(deco::drop_shadow_shape(card, 10, 3, 12, ink.dark, 90));
    }
    deco::outline(p, pts, Stroke::new(1.0, edge));
    if selected {
        chat_paint::pop(p, card, 10, ink, 1.0);
    } else if hov_t > 0.01 {
        chat_paint::pop(p, card, 10, ink, 0.45 * hov_t);
    }
    let _ = off_path;
}

/// Fill and edge of a card: class wash + path tint, or the selected gradient's edge.
fn card_look(ink: &Ink, row: &Row, selected: bool) -> (Color32, Color32) {
    let pal = ink.p;
    if selected {
        let edge = if theme::flat() {
            pal.accent
        } else {
            ink.a.gamma_multiply(0.9)
        };
        return (ink.card(), edge);
    }
    let off_path = !row.on_path;
    if theme::flat() {
        let fill = if off_path { pal.surface } else { pal.raised };
        return (fill, pal.stroke);
    }
    let (fill, edge) = match row.class {
        RowClass::User => (ink.wash(ink.a), ink.edge(ink.a)),
        RowClass::Tool => (
            theme::mix(pal.surface, pal.bg, 0.35),
            theme::mix(ink.line(), pal.weak, 0.2),
        ),
        RowClass::Compaction => (ink.wash(pal.warn), ink.edge(pal.warn)),
        RowClass::Summary => (ink.wash(ink.b), ink.edge(ink.b)),
        RowClass::Assistant | RowClass::Other => (ink.card(), ink.line()),
    };
    if off_path && !matches!(row.class, RowClass::Summary) {
        (
            theme::mix(fill, ink.wash(ink.b), 0.55),
            theme::mix(edge, ink.edge(ink.b), 0.45),
        )
    } else {
        (fill, edge)
    }
}

/// Edges of the rows in `first..last`, plus the long edges crossing that range. Inactive
/// branches first so the active path draws on top.
pub fn paint_graph(
    p: &Painter,
    ink: &Ink,
    layout: &Layout,
    geo: &Geometry<'_>,
    first: usize,
    last: usize,
    scratch: &mut Vec<u32>,
) {
    let pal = ink.p;
    scratch.clear();
    let from = first.saturating_sub(1);
    scratch.extend((from..last).map(|r| r as u32));
    // Long edges whose child row lies outside the window but whose span crosses it.
    for &r in &layout.long_edges {
        let r_us = r as usize;
        if (from..last).contains(&r_us) {
            continue;
        }
        if let Some(parent) = layout.rows[r_us].parent
            && (parent as usize) < last
            && r_us >= from
        {
            scratch.push(r);
        }
    }
    for pass in [false, true] {
        for &r in scratch.iter() {
            let row = &layout.rows[r as usize];
            if row.on_path != pass {
                continue;
            }
            let Some(parent) = row.parent else {
                continue;
            };
            let prow = &layout.rows[parent as usize];
            let (a, b) = (
                geo.at(parent as usize, prow.lane),
                geo.at(r as usize, row.lane),
            );
            let stroke = if row.on_path {
                Stroke::new(2.6, ink.a)
            } else {
                Stroke::new(1.6, lane_color(ink, row.lane))
            };
            edge(p, a, b, stroke, pal);
        }
    }
}

/// Parent `a` → child `b`: straight in a lane, or a smooth S-curve into the child's lane over the
/// first row slot, then straight down to the child.
fn edge(p: &Painter, a: Pos2, b: Pos2, stroke: Stroke, _pal: &Palette) {
    if (a.x - b.x).abs() < 0.5 {
        p.line_segment([a, b], stroke);
        return;
    }
    // The S-curve spans the parent card plus the gap (about one message slot).
    let span = row_height(RowClass::Assistant) + CARD_GAP;
    let end = pos2(b.x, (a.y + span / 2.0).min(b.y));
    let h = end.y - a.y;
    p.add(CubicBezierShape::from_points_stroke(
        [
            a,
            pos2(a.x, a.y + h * 0.62),
            pos2(b.x, end.y - h * 0.62),
            end,
        ],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
    if b.y > end.y {
        p.line_segment([end, b], stroke);
    }
}

/// Ringed node marker: accent on the active path, Violet off it; fork points get an outer ring.
pub fn paint_node(p: &Painter, ink: &Ink, row: &Row, c: Pos2, fork_point: bool) {
    let pal = ink.p;
    let base = pal.bar;
    if row.is_leaf && !theme::flat() {
        p.circle_filled(c, 15.0, ink.a.gamma_multiply(0.14));
        p.circle_filled(c, 10.5, ink.a.gamma_multiply(0.26));
    }
    if fork_point {
        p.circle_stroke(c, 8.5, Stroke::new(1.4, ink.b.gamma_multiply(0.8)));
    }
    if row.on_path {
        p.circle_filled(c, 6.2, base);
        p.circle_filled(c, 4.6, ink.a);
        p.circle_stroke(c, 5.4, Stroke::new(1.0, ink.a.gamma_multiply(0.55)));
        if row.is_leaf {
            p.circle_filled(c, 2.2, Color32::WHITE);
        }
    } else {
        let col = lane_color(ink, row.lane);
        p.circle_filled(c, 6.0, base);
        p.circle_filled(c, 4.4, col);
        p.circle_stroke(c, 5.2, Stroke::new(1.0, ink.b.gamma_multiply(0.6)));
        p.circle_filled(c, 2.1, base);
    }
}
