//! omp's `/btw` side answers. Since omp 19 a native client gets no BTW panel: the answer is a note
//! in the transcript (`omp.btw.note`; `.active` while follow-ups go to it; `.folded` as one line).
//! It is drawn as a block of its own, told apart at a glance from the conversation and from every
//! style's chrome: a hue no style uses for its accents ([`hue`]), a solid rail down its left edge,
//! a tinted header with a `BTW` tag, and the entries of omp's note menu as buttons (the same
//! entries fill its right-click menu).

use egui::ecolor::Hsva;
use egui::{
    Align, Color32, CursorIcon, Layout, Margin, Rect, Sense, Shape, Stroke, Ui, UiBuilder, pos2,
    vec2,
};

use super::ctx::Cx;
use super::render::{self, claim_menu, node};
use super::snap::SNode;
use super::style::Ink;
use super::tools::{self, CardAct, What};
use super::{glyphs, paint};
use crate::{deco, theme};

/// Header height of an open note; a folded note is one such line.
const HEAD: f32 = 30.0;
/// Width of the solid rail down the left edge.
const RAIL: f32 = 3.0;
/// The cut corners (top-right, bottom-left) of the block.
const CUT: f32 = 10.0;
/// Side of one header button.
const BTN: f32 = 24.0;

/// Draws the BTW roles; `false` for any other node.
pub fn role_node(cx: &Cx, ui: &mut Ui, n: &SNode) -> bool {
    match n.role() {
        "omp.btw.note" => note(cx, ui, n, false),
        "omp.btw.note.active" => note(cx, ui, n, true),
        "omp.btw.note.folded" => folded(cx, ui, n),
        _ => return false,
    }
    true
}

/// BTW's own hue: fuchsia, or lime in a style whose accents are fuchsia-like, so the block never
/// reads as the style's accent (nor as IRC's amber). Deeper on light styles.
pub fn hue(ink: &Ink) -> Color32 {
    const FUCHSIA: [Color32; 2] = [
        Color32::from_rgb(0xE8, 0x79, 0xF9),
        Color32::from_rgb(0xA2, 0x1C, 0xAF),
    ];
    const LIME: [Color32; 2] = [
        Color32::from_rgb(0xA3, 0xE6, 0x35),
        Color32::from_rgb(0x4D, 0x7C, 0x0F),
    ];
    let pick = |pair: [Color32; 2]| pair[usize::from(!ink.dark)];
    let fuchsia = pick(FUCHSIA);
    if [ink.p.accent, ink.a, ink.b]
        .into_iter()
        .any(|c| near_hue(fuchsia, c))
    {
        pick(LIME)
    } else {
        fuchsia
    }
}

/// Whether `b` is a colour (not a grey) within 40° of `a`'s hue.
fn near_hue(a: Color32, b: Color32) -> bool {
    let (a, b) = (Hsva::from(a), Hsva::from(b));
    if b.s < 0.25 || b.v < 0.2 {
        return false;
    }
    let d = (a.h - b.h).abs();
    d.min(1.0 - d) < 40.0 / 360.0
}

/// The block's colours.
struct Look {
    hue: Color32,
    fill: Color32,
    edge: Color32,
    head: (Color32, Color32),
    /// The hue as small text/icons on the header tint.
    text: Color32,
}

impl Look {
    fn new(ink: &Ink, active: bool) -> Look {
        let hue = hue(ink);
        let fill = theme::mix(ink.card(), hue, if ink.dark { 0.06 } else { 0.04 });
        let head = (
            theme::mix(ink.p.surface, hue, if ink.dark { 0.26 } else { 0.18 }),
            theme::mix(ink.p.surface, hue, 0.04),
        );
        let edge = theme::mix(ink.p.stroke, hue, if active { 1.0 } else { 0.7 });
        Look {
            hue,
            fill,
            edge: ink.legible(edge, fill, if active { 3.0 } else { 2.0 }),
            head,
            text: ink.legible(hue, head.0, 4.5),
        }
    }
}

/// Registers the click of the whole block on last frame's rect, before its content, so the
/// buttons and text drawn afterwards stay on top of it. Returns whether it was clicked.
fn block_click(ui: &mut Ui, id: egui::Id, enabled: bool) -> bool {
    let Some(rect) = ui.data(|d| d.get_temp::<Rect>(id)) else {
        return false;
    };
    if !enabled {
        return false;
    }
    let resp = ui
        .interact(rect, id.with("click"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    resp.clicked()
}

/// An open note: header (tag, state, buttons), the question, then omp's answer turns and status.
fn note(cx: &Cx, ui: &mut Ui, n: &SNode, active: bool) {
    let ink = cx.ink;
    let look = Look::new(ink, active);
    let acts = tools::card_acts(cx, n);
    let running = acts
        .iter()
        .any(|a| matches!(&a.what, What::Send(s) if s == "btw-stop"));
    let head_node = cx.snap.kids(n).find(|c| c.role() == "omp.btw.note.head");
    let question =
        head_node.and_then(|h| cx.snap.find(h, &|c| c.role() == "omp.btw.note.question"));
    let id = ui.scope_id().with(("btw", n.id.as_str()));
    let hot = hover(ui, id);
    let clicked = block_click(ui, id, n.click().is_some());

    let painter = ui.painter().clone();
    let bg = paint::reserve(&painter);
    let w = ui.available_width();
    let top_left = ui.cursor().min;
    let (head, _) = ui.allocate_exact_size(vec2(w, HEAD), Sense::hover());
    let band = paint::slant_band(
        Rect::from_min_max(
            head.min + vec2(1.0, 1.0),
            pos2(head.right() - CUT, head.bottom()),
        ),
        10.0,
    );
    painter.add(render::gradient_h(&band, look.head.0, look.head.1));
    let buttons_w = buttons_width(&acts);
    let left = Rect::from_min_max(
        pos2(head.left() + RAIL + 10.0, head.top()),
        pos2(head.right() - CUT - buttons_w, head.bottom()),
    );
    let mut lu = ui.new_child(
        UiBuilder::new()
            .id_salt(("btw-head", n.id.as_str()))
            .max_rect(left)
            .layout(Layout::left_to_right(Align::Center)),
    );
    head_left(cx, &mut lu, &look, running, active);
    let mut bu = ui.new_child(
        UiBuilder::new()
            .id_salt(("btw-buttons", n.id.as_str()))
            .max_rect(Rect::from_min_max(
                pos2(left.right(), head.top()),
                pos2(head.right() - CUT, head.bottom()),
            ))
            .layout(Layout::right_to_left(Align::Center)),
    );
    buttons(cx, &mut bu, n, &acts, &look, hot);

    let body = egui::Frame::NONE
        .inner_margin(Margin {
            left: (RAIL + 11.0) as i8,
            right: 12,
            top: 8,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            if let Some(q) = question {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    q_mark(ui, &look);
                    ui.vertical(|ui| node(cx, ui, q));
                });
                let (rule, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 5.0), Sense::hover());
                ui.painter().add(deco::gradient_rule_shape(
                    pos2(rule.left(), rule.center().y),
                    pos2(rule.right(), rule.center().y),
                    1.0,
                    &[(0.0, look.edge), (1.0, look.edge.gamma_multiply(0.0))],
                ));
            }
            for c in cx.snap.kids(n) {
                if c.role() != "omp.btw.note.head" {
                    node(cx, ui, c);
                }
            }
        });

    let total = Rect::from_min_max(top_left, pos2(top_left.x + w, body.response.rect.bottom()));
    paint_frame(ui, bg, total, &look, ink, active, hot);
    ui.data_mut(|d| d.insert_temp(id, total));
    claim_menu(cx, n, total);
    if clicked && let Some(act) = n.click() {
        cx.click(n, act);
    }
}

/// A folded note: one tinted line (chevron, tag, topic, spinner) that opens on a click.
fn folded(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let look = Look::new(ink, false);
    let acts = tools::card_acts(cx, n);
    let id = ui.scope_id().with(("btw", n.id.as_str()));
    let hot = hover(ui, id);
    let clicked = block_click(ui, id, n.click().is_some());

    let painter = ui.painter().clone();
    let bg = paint::reserve(&painter);
    let w = ui.available_width();
    let (line, _) = ui.allocate_exact_size(vec2(w, HEAD), Sense::hover());
    let band = paint::cut_bubble(line.shrink(1.0), CUT - 1.0);
    painter.add(render::gradient_h(&band, look.head.0, look.head.1));
    let buttons_w = buttons_width(&acts);
    let mut lu = ui.new_child(
        UiBuilder::new()
            .id_salt(("btw-line", n.id.as_str()))
            .max_rect(Rect::from_min_max(
                pos2(line.left() + RAIL + 8.0, line.top()),
                pos2(line.right() - CUT - buttons_w, line.bottom()),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    lu.spacing_mut().item_spacing.x = 7.0;
    glyphs::icon(&mut lu, "chev-right", 11.0, look.text, Sense::hover());
    tag(&mut lu, &look);
    for c in cx.snap.kids(n) {
        match c.role() {
            // omp leads the topic with its own `btw` tag: the block's tag stands for it.
            "omp.btw.note.topic" => {
                let plain = super::text::node_plain(c);
                let topic = plain.trim_start().strip_prefix("btw").unwrap_or(&plain);
                lu.add(
                    egui::Label::new(
                        egui::RichText::new(topic.trim())
                            .font(ink.label(12.5))
                            .color(ink.text()),
                    )
                    .truncate()
                    .selectable(false),
                );
            }
            _ if c.kind == "icon" => {}
            _ => node(cx, &mut lu, c),
        }
    }
    let mut bu = ui.new_child(
        UiBuilder::new()
            .id_salt(("btw-buttons", n.id.as_str()))
            .max_rect(Rect::from_min_max(
                pos2(line.right() - CUT - buttons_w, line.top()),
                pos2(line.right() - CUT, line.bottom()),
            ))
            .layout(Layout::right_to_left(Align::Center)),
    );
    buttons(cx, &mut bu, n, &acts, &look, hot);

    paint_frame(ui, bg, line, &look, ink, false, hot);
    ui.data_mut(|d| d.insert_temp(id, line));
    claim_menu(cx, n, line);
    if clicked && let Some(act) = n.click() {
        cx.click(n, act);
    }
}

/// 0..=1 hover of the block (by last frame's rect).
fn hover(ui: &Ui, id: egui::Id) -> f32 {
    let over = ui
        .data(|d| d.get_temp::<Rect>(id))
        .is_some_and(|r| ui.rect_contains_pointer(r));
    ui.ctx().animate_bool_with_time(id.with("hot"), over, 0.12)
}

/// Shadow, tinted cut-corner body with the hue outline (glowing while follow-ups go to the note),
/// the rail, the hue hairline along the top, and the hover pop, into the `bg` slot.
fn paint_frame(
    ui: &Ui,
    bg: egui::layers::ShapeIdx,
    rect: Rect,
    look: &Look,
    ink: &Ink,
    active: bool,
    hot: f32,
) {
    let mut shapes = vec![deco::drop_shadow_shape(rect, 8, 2, 8, ink.dark, 46)];
    if active {
        shapes.push(deco::glow_shape(rect, 6, look.hue.gamma_multiply(0.5), 10));
    }
    shapes.push(Shape::convex_polygon(
        paint::cut_bubble(rect, CUT),
        look.fill,
        Stroke::new(if active { 1.6 } else { 1.0 }, look.edge),
    ));
    shapes.push(Shape::rect_filled(
        Rect::from_min_max(
            rect.left_top(),
            pos2(rect.left() + RAIL, rect.bottom() - CUT),
        ),
        0.0,
        look.hue,
    ));
    shapes.push(deco::gradient_rule_shape(
        pos2(rect.left() + RAIL, rect.top() + 0.5),
        pos2(rect.right() - CUT, rect.top() + 0.5),
        1.0,
        &[(0.0, look.hue), (1.0, look.hue.gamma_multiply(0.15))],
    ));
    if hot > 0.01 {
        shapes.push(Shape::convex_polygon(
            paint::cut_bubble(rect.expand(2.0), CUT + 1.0),
            Color32::TRANSPARENT,
            Stroke::new(1.0, look.hue.gamma_multiply(0.55 * hot)),
        ));
    }
    ui.painter().set(bg, Shape::Vec(shapes));
}

/// Icon, the `BTW` tag, "side question", and the note's state.
fn head_left(cx: &Cx, ui: &mut Ui, look: &Look, running: bool, active: bool) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.x = 7.0;
    glyphs::icon(ui, "help", 14.0, look.text, Sense::hover());
    tag(ui, look);
    ui.label(
        egui::RichText::new("side question")
            .font(ink.label(11.5))
            .color(ink.dim()),
    );
    if running {
        render::spin(cx, ui, 12.0, look.text);
        ui.label(
            egui::RichText::new("answering…")
                .font(ink.label(11.5))
                .color(ink.dim()),
        );
    }
    if active {
        let g = ui.painter().layout_no_wrap(
            "Follow-ups go here · Esc to return".to_owned(),
            ink.label(11.0),
            look.text,
        );
        let (r, _) = ui.allocate_exact_size(g.size() + vec2(14.0, 6.0), Sense::hover());
        paint::boxed(
            ui.painter(),
            r,
            4,
            ink.wash(look.hue),
            Stroke::new(1.0, look.edge),
        );
        ui.painter()
            .galley(r.center() - g.size() / 2.0, g, look.text);
    }
}

/// The solid `BTW` tag.
fn tag(ui: &mut Ui, look: &Look) {
    let font = egui::FontId::new(10.5, egui::FontFamily::Monospace);
    let on = theme::ink_on(look.hue);
    let g = ui.painter().layout_no_wrap("BTW".to_owned(), font, on);
    let (r, _) = ui.allocate_exact_size(g.size() + vec2(12.0, 5.0), Sense::hover());
    ui.painter().add(Shape::convex_polygon(
        paint::parallelogram(r, 3.0),
        look.hue,
        Stroke::NONE,
    ));
    ui.painter().galley(r.center() - g.size() / 2.0, g, on);
}

/// The "Q" mark before the question.
fn q_mark(ui: &mut Ui, look: &Look) {
    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
    paint::boxed(
        ui.painter(),
        r,
        4,
        look.hue.gamma_multiply(0.18),
        Stroke::new(1.0, look.edge),
    );
    ui.painter().text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        "Q",
        egui::FontId::new(11.0, egui::FontFamily::Monospace),
        look.text,
    );
}

fn buttons_width(acts: &[CardAct]) -> f32 {
    acts.len() as f32 * (BTN + 2.0) + 6.0
}

/// One button per entry of omp's note menu, right-aligned in menu order; dim at rest, full while
/// the block is hovered.
fn buttons(cx: &Cx, ui: &mut Ui, n: &SNode, acts: &[CardAct], look: &Look, hot: f32) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.x = 2.0;
    ui.add_space(4.0);
    for act in acts.iter().rev() {
        let sense = if act.disabled {
            Sense::hover()
        } else {
            Sense::click()
        };
        let (r, resp) = ui.allocate_exact_size(vec2(BTN, BTN), sense);
        let over = resp.hovered() && !act.disabled;
        if over {
            paint::boxed(
                ui.painter(),
                r.shrink(1.0),
                4,
                ink.wash(look.hue),
                Stroke::new(1.0, look.edge),
            );
        }
        let base = if act.danger { ink.p.danger } else { look.text };
        let color = if act.disabled {
            ink.faint()
        } else if over {
            base
        } else {
            base.gamma_multiply(0.55 + 0.45 * hot)
        };
        glyphs::paint(ui.painter(), act.icon, r.center(), 13.0, color);
        let tip = match &act.keys {
            Some(k) => format!("{}  ({k})", act.label),
            None => act.label.clone(),
        };
        let resp = resp.on_hover_text(tip);
        if !act.disabled && resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            tools::perform(cx, n, act);
        }
    }
}

#[cfg(test)]
mod tests {
    use deck_core::config::UiStyle;

    use super::*;

    /// The BTW hue stays apart from every style's accents and from IRC's amber, in dark and light.
    #[test]
    fn the_btw_hue_never_matches_a_styles_accents_or_irc() {
        for style in UiStyle::ALL {
            for dark in [true, false] {
                let mut ink = super::super::style::test_ink();
                ink.p = theme::palette(style, dark);
                ink.dark = dark;
                (ink.a, ink.b) = deco::accent_pair(ink.p);
                let h = hue(&ink);
                for (what, c) in [
                    ("accent", ink.p.accent),
                    ("a", ink.a),
                    ("b", ink.b),
                    ("warn", ink.p.warn),
                ] {
                    assert!(
                        !near_hue(h, c),
                        "{style:?} dark={dark}: BTW {h:?} vs {what} {c:?}"
                    );
                }
            }
        }
    }
}
