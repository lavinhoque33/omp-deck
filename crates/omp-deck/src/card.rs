//! "Gradient rule" cards: the chrome of modal dialogs and rich hover cards. A card paints its
//! own frame (fill, soft accent glow, drop shadow and a 1 px border running accent → hairline →
//! second accent at 135°), a header (gradient icon tile, title, optional subtitle and right
//! slot, a soft tone wash and a gradient rule underneath) and a padded body. Body helpers lay out
//! key/value rows, section captions, footer hints and keyboard chips in the same voice.
//!
//! Entry points: [`hover`] (tooltip of a response), [`modal`] (centred dialog with a ✕), or
//! [`show`] inside any frameless `Ui`.

use egui::{
    Align2, Color32, Context, CursorIcon, Frame, Id, Label, Margin, Modal, Rect, Response,
    RichText, Sense, Shape, Sides, Stroke, StrokeKind, Tooltip, Ui, pos2, vec2,
};

use crate::deco;
use crate::icons::{ICON_X, Icon};
use crate::theme::{self, Palette};
use crate::widgets::paint_icon;

/// Which surface the card is: sizes, padding and shadow depth differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Modal,
    Hover,
}

impl Kind {
    /// Horizontal padding of header, body and footer.
    pub fn pad_x(self) -> f32 {
        match self {
            Kind::Modal => f32::from(theme::dialog_margin().left),
            Kind::Hover => 12.0,
        }
    }

    fn head_pad_y(self) -> f32 {
        match self {
            Kind::Modal => theme::dens(13.0, 15.0),
            Kind::Hover => 9.0,
        }
    }

    /// Body padding (top, bottom).
    fn body_pad_y(self) -> (f32, f32) {
        match self {
            Kind::Modal => (theme::dens(14.0, 16.0), theme::dens(14.0, 16.0)),
            Kind::Hover => (10.0, 12.0),
        }
    }

    fn tile(self) -> f32 {
        match self {
            Kind::Modal => 34.0,
            Kind::Hover => 24.0,
        }
    }

    /// Corner radius of a header tile `side` points square.
    fn tile_r(side: f32) -> u8 {
        let r = if side >= 30.0 { 8 } else { 6 };
        r.min(theme::popup_radius() + 3)
    }

    fn title_size(self) -> f32 {
        match self {
            Kind::Modal => 15.5,
            Kind::Hover => 13.0,
        }
    }

    /// Drop shadow `(dy, blur, alpha)`.
    fn shadow(self) -> (i8, u8, u8) {
        match self {
            Kind::Modal => (18, 48, 153),
            Kind::Hover => (12, 32, 140),
        }
    }

    /// Half-axes of the header's tone wash.
    fn wash_reach(self) -> egui::Vec2 {
        match self {
            Kind::Modal => vec2(400.0, 80.0),
            Kind::Hover => vec2(300.0, 60.0),
        }
    }
}

/// Header of a card: icon tile, title, optional subtitle, tone.
#[derive(Clone, Copy, Debug)]
pub struct Header<'a> {
    pub icon: Icon,
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    /// Status tone of the tile, wash and rule: `Color32::TRANSPARENT` = the accent pair, else
    /// e.g. `pal.danger` / `pal.warn` (see [`deco::tone_pair`]).
    pub tone: Color32,
}

impl<'a> Header<'a> {
    pub fn new(icon: Icon, title: &'a str) -> Self {
        Header {
            icon,
            title,
            subtitle: None,
            tone: Color32::TRANSPARENT,
        }
    }

    pub fn subtitle(mut self, subtitle: &'a str) -> Self {
        self.subtitle = Some(subtitle);
        self
    }

    pub fn tone(mut self, tone: Color32) -> Self {
        self.tone = tone;
        self
    }
}

/// Paints a whole card into `ui` (which must have no frame / margin of its own): the header with
/// `right` in its right slot, then `body` in a padded column `width` wide.
pub fn show<R>(
    ui: &mut Ui,
    kind: Kind,
    width: f32,
    head: &Header<'_>,
    right: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui) -> R,
) -> R {
    layout(ui, kind, width, head, right, body).0
}

/// [`show`], also returning the header's rect.
fn layout<R>(
    ui: &mut Ui,
    kind: Kind,
    width: f32,
    head: &Header<'_>,
    right: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui) -> R,
) -> (R, Rect) {
    let pal = Palette::of_ui(ui);
    let dark = ui.visuals().dark_mode;
    let fill = match kind {
        Kind::Modal if !theme::flat() => pal.raised,
        _ => ui.visuals().window_fill,
    };
    let bg = ui.painter().add(Shape::Noop);
    let item_y = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.set_width(width);

    let head_rect = header(ui, kind, head, right);
    let (top, bottom) = kind.body_pad_y();
    let pad_x = kind.pad_x();
    let inner = Frame::NONE
        .inner_margin(Margin {
            left: pad_x as i8,
            right: pad_x as i8,
            top: top as i8,
            bottom: bottom as i8,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = match kind {
                Kind::Modal => item_y.max(8.0),
                Kind::Hover => item_y.max(6.0),
            };
            ui.set_width(width - 2.0 * pad_x);
            body(ui)
        })
        .inner;

    let rect = ui.min_rect();
    let (tone_a, _) = deco::tone_pair(pal, head.tone);
    let wash = Rect::from_min_max(rect.min, pos2(rect.max.x, head_rect.max.y));
    let mut under = frame_under(pal, rect, fill, kind.shadow(), dark);
    under.push(deco::corner_wash(
        wash.shrink(1.0),
        f32::from(theme::popup_radius()),
        kind.wash_reach(),
        tone_a.gamma_multiply(0.14),
    ));
    ui.painter().set(bg, Shape::Vec(under));
    ui.painter().add(frame_outline(pal, rect));
    (inner, head_rect)
}

/// What a card paints under its content: drop shadow `(dy, blur, alpha)`, a soft accent glow and
/// the `fill`, all rounded by [`theme::popup_radius`].
fn frame_under(
    pal: &Palette,
    rect: Rect,
    fill: Color32,
    (dy, blur, alpha): (i8, u8, u8),
    dark: bool,
) -> Vec<Shape> {
    let radius = theme::popup_radius();
    vec![
        deco::drop_shadow_shape(rect, radius, dy, blur, dark, alpha),
        deco::glow_shape(rect, radius, pal.accent.gamma_multiply(0.08), 40),
        Shape::rect_filled(rect, radius, fill),
    ]
}

/// The card's 1 px border: accent → hairline → second accent at 135°.
fn frame_outline(pal: &Palette, rect: Rect) -> Shape {
    let (a, b) = deco::accent_pair(pal);
    deco::gradient_outline_shape(
        deco::rounded_rect(rect, f32::from(theme::popup_radius())),
        1.0,
        &[
            (0.0, a.gamma_multiply(0.7)),
            (0.35, pal.stroke),
            (0.65, pal.stroke),
            (1.0, b.gamma_multiply(0.7)),
        ],
        vec2(1.0, 1.0),
    )
}

/// Gradient-rule chrome for a large surface that is not laid out as a card (a window, the
/// command palette, a picker popup): give its container `Frame::NONE` plus `margin`, and wrap the
/// content in this. Paints the drop shadow, accent glow and `fill` under the content and the
/// gradient border over it, around the content rect grown by `margin`.
pub fn chrome<R>(ui: &mut Ui, margin: Margin, fill: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let pal = Palette::of_ui(ui);
    let dark = ui.visuals().dark_mode;
    // The layer painter is unclipped, so the shadow and glow reach past the window's rect.
    let painter = ui.ctx().layer_painter(ui.layer_id());
    let bg = painter.add(Shape::Noop);
    let r = add(ui);
    let rect = ui.min_rect().union(ui.max_rect()) + margin;
    painter.set(
        bg,
        Shape::Vec(frame_under(pal, rect, fill, Kind::Modal.shadow(), dark)),
    );
    painter.add(frame_outline(pal, rect));
    r
}

/// Gradient icon tile of a header (`side` square, the tone pair of `tone` — `TRANSPARENT` = the
/// accent pair — with a soft glow when `glow`), allocated in `ui`.
pub fn icon_tile(ui: &mut Ui, side: f32, icon: Icon, tone: Color32, glow: bool) -> Response {
    let pal = Palette::of_ui(ui);
    let (ta, tb) = deco::tone_pair(pal, tone);
    let (t, resp) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let r = Kind::tile_r(side);
    let p = ui.painter();
    if glow {
        deco::glow(p, t, r, ta.gamma_multiply(0.45), 16);
    }
    deco::fill_linear(
        p,
        &deco::rounded_rect(t, f32::from(r)),
        ta,
        tb,
        vec2(1.0, 1.0),
    );
    paint_icon(
        p,
        t.center(),
        Align2::CENTER_CENTER,
        icon,
        (side * 0.5).round().max(13.0),
        deco::ink_on_pair(ta, tb),
    );
    resp
}

/// The gradient rule under a header, from `left` to `right` at `y`: tone → second accent → fade.
pub fn header_rule(ui: &Ui, left: f32, right: f32, y: f32, tone: Color32) {
    ui.painter()
        .add(rule_shape(Palette::of_ui(ui), left, right, y, tone));
}

fn rule_shape(pal: &Palette, left: f32, right: f32, y: f32, tone: Color32) -> Shape {
    let (ta, _) = deco::tone_pair(pal, tone);
    deco::gradient_rule_shape(
        pos2(left, y),
        pos2(right, y),
        1.0,
        &[
            (0.0, ta),
            (0.5, deco::accent_pair(pal).1.gamma_multiply(0.6)),
            (1.0, Color32::TRANSPARENT),
        ],
    )
}

/// The header band; returns its rect (the gradient rule sits on its bottom edge).
fn header(ui: &mut Ui, kind: Kind, head: &Header<'_>, right: impl FnOnce(&mut Ui)) -> Rect {
    let pal = Palette::of_ui(ui);
    let pad_x = kind.pad_x();
    let pad_y = kind.head_pad_y();
    let tile = kind.tile();
    let rect = Frame::NONE
        .inner_margin(Margin::symmetric(pad_x as i8, pad_y as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            Sides::new()
                .height(tile)
                .spacing(10.0)
                .shrink_left()
                .truncate()
                .show(
                    ui,
                    |ui| {
                        ui.spacing_mut().item_spacing.x =
                            if kind == Kind::Modal { 12.0 } else { 9.0 };
                        icon_tile(ui, tile, head.icon, head.tone, kind == Kind::Modal);
                        // A fixed-height column so the layout centres it on the tile.
                        let family = egui::TextStyle::Body.resolve(ui.style()).family;
                        let row = |ui: &Ui, size: f32| {
                            ui.fonts_mut(|f| f.row_height(&egui::FontId::new(size, family.clone())))
                        };
                        let mut h = row(ui, kind.title_size());
                        if head.subtitle.is_some() {
                            h += 1.0 + row(ui, 12.0);
                        }
                        let w = ui.available_width();
                        ui.allocate_ui_with_layout(
                            vec2(w, h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.spacing_mut().item_spacing.y = 1.0;
                                ui.add(
                                    Label::new(
                                        theme::strong(head.title)
                                            .size(kind.title_size())
                                            .color(pal.text_strong),
                                    )
                                    .truncate()
                                    .selectable(false),
                                );
                                if let Some(s) = head.subtitle {
                                    ui.add(
                                        Label::new(RichText::new(s).size(12.0).color(pal.weak))
                                            .truncate()
                                            .selectable(false),
                                    );
                                }
                            },
                        );
                    },
                    right,
                );
        })
        .response
        .rect;
    let y = rect.bottom() - 0.5;
    ui.painter().add(rule_shape(
        pal,
        rect.left() + pad_x,
        rect.right() - pad_x,
        y,
        head.tone,
    ));
    rect
}

/// Hover card of `resp` (shown while it is hovered): `width` wide, below/above the widget or,
/// with `at_pointer`, next to the pointer. `None` while hidden.
pub fn hover<R>(
    resp: &Response,
    width: f32,
    at_pointer: bool,
    head: &Header<'_>,
    right: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let mut tip = Tooltip::for_enabled(resp);
    if at_pointer {
        tip = tip.at_pointer().gap(12.0);
    }
    tip.popup = tip.popup.frame(Frame::NONE);
    tip.show(|ui| show(ui, Kind::Hover, width, head, right, body))
        .map(|r| r.inner)
}

/// Centred modal dialog card with a ✕ in the header; `body` lays out the content and usually
/// ends with [`footer`]. Returns whether the dialog was dismissed this frame (✕, a click on the
/// backdrop, or Escape).
pub fn modal(
    ctx: &Context,
    id: Id,
    width: f32,
    head: &Header<'_>,
    body: impl FnOnce(&mut Ui),
) -> bool {
    let m = Modal::new(id)
        .backdrop_color(theme::backdrop(ctx.global_style().visuals.dark_mode))
        .frame(Frame::NONE)
        .show(ctx, |ui| {
            let mut closed = false;
            show(
                ui,
                Kind::Modal,
                width,
                head,
                |ui| closed = close_button(ui).clicked(),
                body,
            );
            closed
        });
    m.inner || m.should_close()
}

/// The header ✕ of a modal.
pub fn close_button(ui: &mut Ui) -> Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let r = theme::ui_rad(4);
    p.rect_filled(rect, r, theme::hover_fill(pal).gamma_multiply(hov));
    if hov > 0.0 {
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, pal.hover_line.gamma_multiply(hov)),
            StrokeKind::Inside,
        );
    }
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        ICON_X,
        14.0,
        theme::mix(pal.weak, pal.text_strong, hov),
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Close (Esc)")
}

/// Key / value row: accent marker + UPPERCASE tracked key in a `key_w` column, then `value`.
pub fn kv<R>(ui: &mut Ui, key_w: f32, key: &str, value: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        key_cell(ui, key_w, key);
        ui.spacing_mut().item_spacing.x = 6.0;
        value(ui)
    })
    .inner
}

fn key_cell(ui: &mut Ui, key_w: f32, key: &str) {
    let pal = Palette::of_ui(ui);
    let galley = ui.painter().layout_job(tracked(key, 11.0, 1.1, pal.glyph));
    let h = galley.size().y.max(18.0);
    // `key_w` aligns the rows of a card; a key wider than it (another style's label font) grows
    // its own cell rather than running into the value.
    let w = key_w.max(10.0 + galley.size().x + 8.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    let c = pos2(rect.left() + 2.5, rect.center().y);
    p.add(Shape::convex_polygon(
        vec![
            c + vec2(-2.0, -3.0),
            c + vec2(2.5, 0.0),
            c + vec2(-2.0, 3.0),
        ],
        pal.accent,
        Stroke::NONE,
    ));
    let pos = pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0);
    p.with_clip_rect(rect).galley(pos, galley, pal.glyph);
}

/// UPPERCASE letter-spaced text in the label font.
fn tracked(text: &str, size: f32, spacing: f32, color: Color32) -> egui::text::LayoutJob {
    egui::text::LayoutJob::single_section(
        text.to_uppercase(),
        egui::TextFormat {
            font_id: theme::label_font(size),
            extra_letter_spacing: spacing,
            color,
            ..Default::default()
        },
    )
}

/// Section caption inside a card body: UPPERCASE tracked weak text and a rule fading out to the
/// right.
pub fn section(ui: &mut Ui, title: &str) {
    let pal = Palette::of_ui(ui);
    let galley = ui.painter().layout_job(tracked(title, 10.5, 1.0, pal.weak));
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), galley.size().y + 2.0),
        Sense::hover(),
    );
    let p = ui.painter();
    let text_w = galley.size().x;
    p.galley(
        pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        pal.weak,
    );
    let x = rect.left() + text_w + 8.0;
    if x < rect.right() {
        deco::gradient_rule(
            p,
            pos2(x, rect.center().y),
            pos2(rect.right(), rect.center().y),
            1.0,
            &[(0.0, pal.stroke), (1.0, Color32::TRANSPARENT)],
        );
    }
}

/// Keyboard chip: raised fill, hairline border with a 2 px bottom edge.
pub fn kbd(ui: &mut Ui, keys: &str) -> Response {
    let pal = Palette::of_ui(ui);
    let galley = ui
        .painter()
        .layout_job(egui::text::LayoutJob::single_section(
            keys.to_owned(),
            egui::TextFormat {
                font_id: theme::label_font(10.5),
                color: pal.text,
                ..Default::default()
            },
        ));
    let size = galley.size() + vec2(10.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    let r = theme::ui_rad(3);
    p.rect_filled(rect, r, pal.raised);
    p.rect_stroke(rect, r, Stroke::new(1.0, pal.stroke), StrokeKind::Inside);
    p.line_segment(
        [
            pos2(rect.left() + 1.0, rect.bottom() - 1.0),
            pos2(rect.right() - 1.0, rect.bottom() - 1.0),
        ],
        Stroke::new(1.0, pal.stroke),
    );
    p.galley(
        rect.center() - galley.size() / 2.0 - vec2(0.0, 0.5),
        galley,
        pal.text,
    );
    resp
}

/// Footer row of hints: `kbd` chip + weak explanation per pair.
pub fn footer_hints(ui: &mut Ui, hints: &[(&str, &str)]) {
    ui.add_space(2.0);
    ui.horizontal(|ui| hint_row(ui, hints));
}

fn hint_row(ui: &mut Ui, hints: &[(&str, &str)]) {
    let pal = Palette::of_ui(ui);
    for (i, (keys, what)) in hints.iter().enumerate() {
        if i > 0 {
            ui.add_space(8.0);
        }
        ui.spacing_mut().item_spacing.x = 5.0;
        kbd(ui, keys);
        ui.label(RichText::new(*what).size(11.0).color(pal.weak));
    }
}

/// Modal footer: keyboard `hints` on the left, `buttons` laid out right → left (add the confirm
/// button first so it ends up rightmost).
pub fn footer(ui: &mut Ui, hints: &[(&str, &str)], buttons: impl FnOnce(&mut Ui)) {
    ui.add_space(theme::dens(4.0, 8.0));
    Sides::new()
        .height(crate::widgets::draft_height() + 6.0)
        .show(
            ui,
            |ui| hint_row(ui, hints),
            |ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                buttons(ui);
            },
        );
    ui.add_space(crate::widgets::DRAFT_BELOW);
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::config::{Density, UiFont, UiStyle};

    /// In every style and density both kinds keep modest padding on all four sides of the body
    /// (top measured from the header's rule), so content never sits flush against the border.
    #[test]
    fn body_keeps_padding_on_all_sides() {
        let ctx = Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);
        for style in UiStyle::ALL {
            for density in [Density::Compact, Density::Comfortable] {
                theme::apply(&ctx, style, density, UiFont::Auto, true);
                for kind in [Kind::Modal, Kind::Hover] {
                    let (mut card, mut head, mut body) =
                        (Rect::NOTHING, Rect::NOTHING, Rect::NOTHING);
                    ctx.run_ui(egui::RawInput::default(), |ui| {
                        egui::Area::new(Id::unique("card-test")).show(ui.ctx(), |ui| {
                            let ((), h) = layout(
                                ui,
                                kind,
                                320.0,
                                &Header::new(ICON_X, "Title").subtitle("Sub"),
                                |_| {},
                                |ui| {
                                    ui.label("body");
                                    body = ui.min_rect();
                                },
                            );
                            head = h;
                            card = ui.min_rect();
                        });
                    })
                    .textures_delta
                    .clear();
                    let pad = [
                        body.left() - card.left(),
                        body.top() - head.bottom(),
                        card.right() - body.right(),
                        card.bottom() - body.bottom(),
                    ];
                    for (side, v) in ["left", "top", "right", "bottom"].iter().zip(pad) {
                        assert!(
                            (6.0..=24.0).contains(&v),
                            "{style:?} {density:?} {kind:?} {side} padding {v}"
                        );
                    }
                }
            }
        }
    }
}
