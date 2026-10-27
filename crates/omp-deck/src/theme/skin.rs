//! Painted surfaces of the style gallery: window regions (Lines / Islands / Frame / Fade), canvas
//! decoration, card chrome, tab and row chrome. Everything here reads the active [`Look`] and
//! [`Palette`]; meshes are built once per geometry and cached in the egui context.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::epaint::Mesh;
use egui::{
    Color32, Context, CornerRadius, Frame, Id, Margin, Painter, Pos2, Rect, Shadow, Shape, Stroke,
    StrokeKind, pos2, vec2,
};

use super::{Active, Canvas, Hover, Palette, Region, TabKind, look, mix};

// ------------------------------------------------------------------ colours and meshes

/// `c` at `alpha` (0..=255) as a translucent colour.
pub fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// White at `a` (0..=255).
pub fn white(a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, a)
}

/// Black at `a` (0..=255).
pub fn black(a: u8) -> Color32 {
    Color32::from_black_alpha(a)
}

#[derive(Clone)]
struct Cached {
    sig: u64,
    mesh: Arc<Mesh>,
}

fn sig_of(parts: &impl Hash) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

fn rect_bits(r: Rect) -> [u32; 4] {
    [
        r.min.x.to_bits(),
        r.min.y.to_bits(),
        r.max.x.to_bits(),
        r.max.y.to_bits(),
    ]
}

/// The mesh stored under `id`, rebuilt by `build` when `sig` changed.
fn cached_mesh(ctx: &Context, id: Id, sig: u64, build: impl FnOnce() -> Mesh) -> Arc<Mesh> {
    if let Some(c) = ctx.data(|d| d.get_temp::<Cached>(id))
        && c.sig == sig
    {
        return c.mesh;
    }
    let mesh = Arc::new(build());
    ctx.data_mut(|d| {
        d.insert_temp(
            id,
            Cached {
                sig,
                mesh: Arc::clone(&mesh),
            },
        );
    });
    mesh
}

/// Triangle fan over a rounded rectangle with radii `[nw, ne, se, sw]`; `color` must be an
/// affine function of the position so the fan interpolates it exactly (linear gradients).
pub fn round_mesh(rect: Rect, radii: [f32; 4], color: impl Fn(Pos2) -> Color32) -> Mesh {
    let limit = (rect.height() / 2.0).min(rect.width() / 2.0).max(0.0);
    let [nw, ne, se, sw] = radii.map(|r| r.min(limit).max(0.0));
    let mut m = Mesh::default();
    m.colored_vertex(rect.center(), color(rect.center()));
    let corners = [
        (rect.right_top() + vec2(-ne, ne), ne, -90.0f32),
        (rect.right_bottom() + vec2(-se, -se), se, 0.0),
        (rect.left_bottom() + vec2(sw, -sw), sw, 90.0),
        (rect.left_top() + vec2(nw, nw), nw, 180.0),
    ];
    for (c, r, start) in corners {
        let steps = if r < 1.0 { 1 } else { 8 };
        for i in 0..=steps {
            let ang = (start + 90.0 * i as f32 / steps as f32).to_radians();
            let p = c + r * vec2(ang.cos(), ang.sin());
            m.colored_vertex(p, color(p));
        }
    }
    let n = (m.vertices.len() - 1) as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    m
}

/// Direction of a linear gradient across a rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Down,
    Right,
    Diagonal,
}

/// Linear gradient from `a` (top-left) to `b` over a rounded rectangle, cached under `id`.
pub fn gradient_card(
    painter: &Painter,
    id: Id,
    rect: Rect,
    radius: f32,
    colors: (Color32, Color32),
    dir: Dir,
) {
    gradient_corners(painter, id, rect, [radius; 4], colors, dir);
}

/// [`gradient_card`] with per-corner radii `[nw, ne, se, sw]`.
pub fn gradient_corners(
    painter: &Painter,
    id: Id,
    rect: Rect,
    radii: [f32; 4],
    (a, b): (Color32, Color32),
    dir: Dir,
) {
    if a == b {
        let cr = CornerRadius {
            nw: radii[0] as u8,
            ne: radii[1] as u8,
            se: radii[2] as u8,
            sw: radii[3] as u8,
        };
        painter.rect_filled(rect, cr, a);
        return;
    }
    let sig = sig_of(&(rect_bits(rect), radii.map(f32::to_bits), a, b, dir));
    let mesh = cached_mesh(painter.ctx(), id.with("grad"), sig, || {
        let size = rect.size();
        round_mesh(rect, radii, |p| {
            let fx = ((p.x - rect.min.x) / size.x.max(1.0)).clamp(0.0, 1.0);
            let fy = ((p.y - rect.min.y) / size.y.max(1.0)).clamp(0.0, 1.0);
            let t = match dir {
                Dir::Down => fy,
                Dir::Right => fx,
                Dir::Diagonal => (fx + fy) / 2.0,
            };
            mix(a, b, t)
        })
    });
    painter.add(Shape::mesh(mesh));
}

/// Elliptical glow: `color` at the centre fading linearly to transparent at `(rx, ry)`.
fn radial_mesh(center: Pos2, rx: f32, ry: f32, color: Color32) -> Mesh {
    const N: usize = 40;
    let mut m = Mesh::default();
    m.colored_vertex(center, color);
    for i in 0..N {
        let a = std::f32::consts::TAU * i as f32 / N as f32;
        m.colored_vertex(
            center + vec2(rx * a.cos(), ry * a.sin()),
            Color32::TRANSPARENT,
        );
    }
    for i in 0..N as u32 {
        m.add_triangle(0, 1 + i, 1 + (i + 1) % N as u32);
    }
    m
}

fn glow(painter: &Painter, id: Id, center: Pos2, (rx, ry): (f32, f32), color: Color32) {
    let sig = sig_of(&(
        center.x.to_bits(),
        center.y.to_bits(),
        rx.to_bits(),
        ry.to_bits(),
        color,
    ));
    let mesh = cached_mesh(painter.ctx(), id, sig, || {
        radial_mesh(center, rx, ry, color)
    });
    painter.add(Shape::mesh(mesh));
}

/// Fading 1 px rule (Spotlight's divider above the accordion header).
pub fn fade_rule(painter: &Painter, id: Id, x0: f32, x1: f32, y: f32) {
    fade_line(painter, pos2(x0, y), pos2(x1, y), rgb_of(0x3a3a3a), id);
}

/// 1 px line fading to transparent at both ends (Spotlight separators).
fn fade_line(painter: &Painter, a: Pos2, b: Pos2, color: Color32, id: Id) {
    let sig = sig_of(&(
        a.x.to_bits(),
        a.y.to_bits(),
        b.x.to_bits(),
        b.y.to_bits(),
        color,
    ));
    let mesh = cached_mesh(painter.ctx(), id, sig, || {
        let mut m = Mesh::default();
        let (dx, dy) = if (b.x - a.x).abs() >= (b.y - a.y).abs() {
            (0.0, 0.5)
        } else {
            (0.5, 0.0)
        };
        let off = vec2(dx, dy);
        let stops = [0.0, 0.15, 0.85, 1.0];
        for (i, t) in stops.into_iter().enumerate() {
            let c = if i == 0 || i == 3 {
                Color32::TRANSPARENT
            } else {
                color
            };
            let p = a + (b - a) * t;
            m.colored_vertex(p - off, c);
            m.colored_vertex(p + off, c);
        }
        for i in 0..3u32 {
            let k = i * 2;
            m.add_triangle(k, k + 1, k + 2);
            m.add_triangle(k + 1, k + 3, k + 2);
        }
        m
    });
    painter.add(Shape::mesh(mesh));
}

fn dashed_h(painter: &Painter, x0: f32, x1: f32, y: f32, color: Color32, (dash, gap): (f32, f32)) {
    let mut x = x0;
    let stroke = Stroke::new(1.0, color);
    while x < x1 {
        painter.hline(x..=(x + dash).min(x1), y, stroke);
        x += dash + gap;
    }
}

fn dashed_v(painter: &Painter, x: f32, y0: f32, y1: f32, color: Color32, (dash, gap): (f32, f32)) {
    let mut y = y0;
    let stroke = Stroke::new(1.0, color);
    while y < y1 {
        painter.vline(x, y..=(y + dash).min(y1), stroke);
        y += dash + gap;
    }
}

/// Dashed horizontal rule.
pub fn dashed_hline(painter: &Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    dashed_h(painter, x0, x1, y, color, (4.0, 3.0));
}

/// Dashed vertical rule.
pub fn dashed_vline(painter: &Painter, x: f32, y0: f32, y1: f32, color: Color32) {
    dashed_v(painter, x, y0, y1, color, (4.0, 3.0));
}

/// 1 px inner highlight along the top edge, inset by the corner radius.
pub fn top_highlight(painter: &Painter, rect: Rect, radius: f32, a: u8) {
    if a == 0 {
        return;
    }
    let inset = radius.max(1.0) * 0.7;
    painter.hline(
        (rect.left() + inset)..=(rect.right() - inset),
        rect.top() + 1.0,
        Stroke::new(1.0, white(a)),
    );
}

// ------------------------------------------------------------------ window regions

/// Backdrop of the whole window (what shows in the Islands gaps and around the Frame canvas).
/// Separated panels sit on the bar tone, so the gaps read as part of the title and status bars.
pub fn paint_backdrop(painter: &Painter, screen: Rect, pal: &Palette) {
    let fill = if look().separated {
        pal.bar
    } else {
        pal.window
    };
    painter.rect_filled(screen, 0, fill);
}

/// Gap between the Islands panels and the window edge.
fn gap() -> i8 {
    look().island_gap as i8
}

/// An Islands side panel: rounded, bordered, with the designed drop shadow (none when separated).
fn island_panel(pal: &Palette, outer: Margin, inner: Margin) -> Frame {
    let lk = look();
    Frame::NONE
        .fill(pal.panel)
        .stroke(Stroke::new(1.0, pal.separator))
        .corner_radius(lk.island_r)
        .shadow(if lk.separated {
            Shadow::NONE
        } else {
            Shadow {
                offset: [0, 10],
                blur: 30,
                spread: 0,
                color: black(140),
            }
        })
        .outer_margin(outer)
        .inner_margin(inner)
}

/// Frame of the sidebar panel; `inner` is the inner margin of the flat look.
pub fn sidebar_frame(pal: &Palette, inner: Margin) -> Frame {
    match look().region {
        Region::Islands => island_panel(
            pal,
            Margin {
                left: gap(),
                right: gap() / 2,
                top: 2,
                bottom: 2,
            },
            inner,
        ),
        _ => Frame::NONE.fill(pal.panel).inner_margin(inner),
    }
}

/// Frame of a right-hand side panel (Changes): an Islands panel beside the canvas (whose own
/// margin is the gap), else flat.
pub fn right_panel_frame(pal: &Palette, inner: Margin) -> Frame {
    match look().region {
        Region::Islands => island_panel(
            pal,
            Margin {
                left: 0,
                right: gap(),
                top: 2,
                bottom: 2,
            },
            inner,
        ),
        _ => Frame::NONE.fill(pal.panel).inner_margin(inner),
    }
}

/// Whether egui should draw its own separator line next to the sidebar panel.
pub fn panel_separator_line() -> bool {
    look().region == Region::Lines
}

/// Whether egui should draw its separator line next to a right-hand side panel (Islands panels
/// carry their own border).
pub fn right_panel_separator_line() -> bool {
    look().region != Region::Islands
}

/// Frame of the central panel: the canvas, rounded and bordered where the region asks for it.
pub fn central_frame(pal: &Palette) -> Frame {
    match look().region {
        Region::Islands => Frame::NONE
            .fill(pal.bg)
            .stroke(Stroke::new(1.0, pal.separator))
            .corner_radius(look().island_r)
            .outer_margin(Margin {
                left: gap() / 2,
                right: gap(),
                top: 2,
                bottom: 2,
            }),
        Region::Frame => Frame::NONE.fill(pal.bg).corner_radius(CornerRadius {
            nw: 14,
            ne: 0,
            sw: 0,
            se: 0,
        }),
        _ => Frame::NONE.fill(pal.bg),
    }
}

/// Shows the central panel around `add`: the canvas fill and region frame (gallery styles, and
/// Neutral and Neon with separated panels; otherwise those two keep their unfilled panel), no
/// scroll-edge fade over the canvas, and the region's decoration painted above the content.
/// `terminal` keeps the canvas clear of rounded corners around a terminal.
pub fn central<R>(
    ui: &mut egui::Ui,
    pal: &Palette,
    terminal: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let framed = super::is_gallery() || look().separated;
    let frame = if framed {
        let mut f = central_frame(pal);
        if terminal {
            f = f.inner_margin(terminal_margin());
        }
        f
    } else {
        Frame::NONE
    };
    let out = egui::CentralPanel::default_margins()
        .frame(frame)
        .show(ui, |ui| {
            ui.spacing_mut().scroll.fade.strength = 0.0;
            add(ui)
        });
    if framed {
        paint_central_decor(ui.painter(), out.response.rect, pal);
        paint_sidebar_cast(ui.painter(), out.response.rect);
    }
    out
}

/// Margin around the terminal inside the central panel so it keeps clear of rounded corners.
pub fn terminal_margin() -> Margin {
    match look().region {
        Region::Islands => Margin::same((look().island_r / 2) as i8),
        Region::Frame => Margin {
            left: 4,
            top: 4,
            right: 0,
            bottom: 0,
        },
        _ => Margin::ZERO,
    }
}

/// Decoration above the central panel's content: the Frame canvas' top/left highlight and inner
/// shadow, the Islands panel highlight.
pub fn paint_central_decor(painter: &Painter, rect: Rect, pal: &Palette) {
    match look().region {
        Region::Frame => {
            let id = Id::unique("skin-frame-decor");
            let edge = mix(pal.window, white(255), 0.07);
            let sig = sig_of(&(rect_bits(rect), edge));
            let mesh = cached_mesh(painter.ctx(), id, sig, || {
                let mut m = Mesh::default();
                // Inner shadow under the canvas' top edge.
                let h = 18.0;
                let top = rect.top();
                let c0 = black(150);
                let c1 = Color32::TRANSPARENT;
                m.colored_vertex(pos2(rect.left() + 14.0, top), c0);
                m.colored_vertex(pos2(rect.right(), top), c0);
                m.colored_vertex(pos2(rect.left() + 14.0, top + h), c1);
                m.colored_vertex(pos2(rect.right(), top + h), c1);
                m.add_triangle(0, 1, 2);
                m.add_triangle(1, 3, 2);
                // 1 px highlight along the left edge, round NW corner and top edge.
                let r = 14.0;
                let (l, t) = (rect.left() + 0.5, rect.top() + 0.5);
                let mut pts = [Pos2::ZERO; 11];
                pts[0] = pos2(l, rect.bottom());
                pts[1] = pos2(l, t + r);
                for (i, p) in pts.iter_mut().enumerate().skip(1).take(9) {
                    let ang =
                        std::f32::consts::PI + std::f32::consts::FRAC_PI_2 * (i - 1) as f32 / 8.0;
                    *p = pos2(l + r + r * ang.cos(), t + r + r * ang.sin());
                }
                pts[10] = pos2(rect.right(), t);
                for seg in pts.windows(2) {
                    let (a, b) = (seg[0], seg[1]);
                    let d = b - a;
                    let n = if d.length() > 0.0 {
                        vec2(-d.y, d.x).normalized() * 0.5
                    } else {
                        vec2(0.0, 0.5)
                    };
                    let base = m.vertices.len() as u32;
                    for p in [a - n, a + n, b - n, b + n] {
                        m.colored_vertex(p, edge);
                    }
                    m.add_triangle(base, base + 1, base + 2);
                    m.add_triangle(base + 1, base + 3, base + 2);
                }
                m
            });
            painter.add(Shape::mesh(mesh));
        }
        Region::Islands if !look().separated => {
            painter.rect_stroke(
                rect.shrink(1.0),
                look().island_r - 1,
                Stroke::new(1.0, white(14)),
                StrokeKind::Inside,
            );
        }
        _ => {}
    }
}

/// Which edge of a bar a separator sits on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Right,
}

/// Region separator along `edge` of `rect` (tab row bottom, status bar top, sidebar right).
pub fn paint_edge(painter: &Painter, rect: Rect, edge: Edge, pal: &Palette) {
    let lk = look();
    let w = lk.sep_w;
    match lk.region {
        Region::Lines => {
            let s = Stroke::new(w, pal.separator);
            match edge {
                Edge::Top => {
                    painter.hline(rect.x_range(), rect.top() + w / 2.0, s);
                }
                Edge::Bottom => {
                    painter.hline(rect.x_range(), rect.bottom() - w / 2.0, s);
                }
                Edge::Right => {
                    painter.vline(rect.right() - w / 2.0, rect.y_range(), s);
                }
            }
        }
        Region::Fade => {
            let id = Id::unique(("skin-fade-edge", edge as u8));
            let col = if edge == Edge::Top {
                rgb_of(0x3a3a3a)
            } else {
                rgb_of(0x4a4a4a)
            };
            match edge {
                Edge::Top => {
                    fade_line(
                        painter,
                        pos2(rect.left(), rect.top() + 0.5),
                        pos2(rect.right(), rect.top() + 0.5),
                        col,
                        id,
                    );
                }
                Edge::Bottom => {
                    fade_line(
                        painter,
                        pos2(rect.left(), rect.bottom() - 0.5),
                        pos2(rect.right(), rect.bottom() - 0.5),
                        col,
                        id,
                    );
                }
                Edge::Right => {
                    fade_line(
                        painter,
                        pos2(rect.right() - 0.5, rect.top()),
                        pos2(rect.right() - 0.5, rect.bottom()),
                        col,
                        id,
                    );
                }
            }
        }
        // Islands carry their own border; Frame separates by tone alone.
        Region::Islands | Region::Frame => {}
    }
    // Teal Signal's accent line under the tab row.
    if edge == Edge::Bottom && lk.tab_rule {
        painter.hline(
            rect.x_range(),
            rect.bottom() - 1.5,
            Stroke::new(1.0, alpha(pal.accent, 46)),
        );
    }
}

/// Shadow the sidebar casts onto the canvas (Layered Depth).
fn paint_sidebar_cast(painter: &Painter, canvas: Rect) {
    if !look().cast {
        return;
    }
    let id = Id::unique("skin-sidebar-cast");
    let rect = Rect::from_min_size(canvas.left_top(), vec2(22.0, canvas.height()));
    let sig = sig_of(&rect_bits(rect));
    let mesh = cached_mesh(painter.ctx(), id, sig, || {
        let mut m = Mesh::default();
        m.colored_vertex(rect.left_top(), black(112));
        m.colored_vertex(rect.right_top(), Color32::TRANSPARENT);
        m.colored_vertex(rect.left_bottom(), black(112));
        m.colored_vertex(rect.right_bottom(), Color32::TRANSPARENT);
        m.add_triangle(0, 1, 2);
        m.add_triangle(1, 3, 2);
        m
    });
    painter.add(Shape::mesh(mesh));
}

const fn rgb_of(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// ------------------------------------------------------------------ canvas

/// Decoration behind non-terminal main content (the fill itself is the central frame's).
pub fn paint_canvas(painter: &Painter, rect: Rect, pal: &Palette) {
    let painter = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    let id = Id::unique(("skin-canvas", super::style().key()));
    let w = rect.width();
    let h = rect.height();
    match look().canvas {
        Canvas::Flat => {}
        Canvas::Dots => {
            let sig = sig_of(&rect_bits(rect));
            let mesh = cached_mesh(painter.ctx(), id, sig, || {
                let dot = Color32::from_rgba_unmultiplied(148, 163, 184, 56);
                let mut m = Mesh::default();
                let mut y = rect.top() + 9.0;
                while y < rect.bottom() {
                    let mut x = rect.left() + 9.0;
                    while x < rect.right() {
                        let r = Rect::from_center_size(pos2(x.round(), y.round()), vec2(1.6, 1.6));
                        m.add_colored_rect(r, dot);
                        x += 18.0;
                    }
                    y += 18.0;
                }
                m
            });
            painter.add(Shape::mesh(mesh));
        }
        Canvas::Scanlines => {
            let sig = sig_of(&rect_bits(rect));
            let mesh = cached_mesh(painter.ctx(), id, sig, || {
                let c = Color32::from_rgba_unmultiplied(34, 197, 94, 12);
                let mut m = Mesh::default();
                let mut y = rect.top();
                while y < rect.bottom() {
                    m.add_colored_rect(
                        Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), 1.0)),
                        c,
                    );
                    y += 3.0;
                }
                m
            });
            painter.add(Shape::mesh(mesh));
        }
        Canvas::Aurora => {
            let violet = Color32::from_rgba_unmultiplied(167, 139, 250, 41);
            let cyan = Color32::from_rgba_unmultiplied(34, 211, 238, 26);
            glow(
                &painter,
                id.with(0),
                pos2(rect.left() + w * 0.85, rect.top() - h * 0.08),
                (490.0, 224.0),
                violet,
            );
            glow(
                &painter,
                id.with(1),
                pos2(rect.left() + w * 0.35, rect.top() - h * 0.12),
                (420.0, 210.0),
                cyan,
            );
        }
        Canvas::Glass => {
            glow(
                &painter,
                id.with(0),
                pos2(rect.left() + w * 0.6, rect.top() - h * 0.15),
                (630.0, 294.0),
                white(18),
            );
            glow(
                &painter,
                id.with(1),
                pos2(rect.right(), rect.bottom()),
                (350.0, 182.0),
                alpha(pal.accent, 15),
            );
        }
        Canvas::Rose => {
            glow(
                &painter,
                id.with(0),
                pos2(rect.left() + w * 0.8, rect.top() - h * 0.1),
                (490.0, 210.0),
                alpha(pal.accent, 26),
            );
        }
        Canvas::Spotlight => {
            glow(
                &painter,
                id.with(0),
                pos2(rect.center().x, rect.top() - h * 0.2),
                (630.0, 350.0),
                white(13),
            );
        }
        Canvas::Wash { alpha: a, height } => {
            let strip = Rect::from_min_size(rect.min, vec2(w, f32::from(height)));
            let sig = sig_of(&(rect_bits(strip), a, pal.accent));
            let accent = pal.accent;
            let mesh = cached_mesh(painter.ctx(), id, sig, || {
                let mut m = Mesh::default();
                m.colored_vertex(strip.left_top(), alpha(accent, a));
                m.colored_vertex(strip.right_top(), alpha(accent, a));
                m.colored_vertex(strip.left_bottom(), Color32::TRANSPARENT);
                m.colored_vertex(strip.right_bottom(), Color32::TRANSPARENT);
                m.add_triangle(0, 1, 2);
                m.add_triangle(1, 3, 2);
                m
            });
            painter.add(Shape::mesh(mesh));
        }
    }
}

/// Spotlight behind the active card (no-op in the other styles).
pub fn paint_spot(painter: &Painter, card: Rect, pal: &Palette) {
    if look().canvas != Canvas::Spotlight {
        return;
    }
    glow(
        painter,
        Id::unique(("skin-spot", super::style().key())),
        card.center(),
        (
            (card.width() * 0.75).max(260.0),
            (card.height() * 0.75).max(210.0),
        ),
        alpha(pal.accent, 30),
    );
}

// ------------------------------------------------------------------ cards

/// What a card is doing.
#[derive(Clone, Copy, Default)]
pub struct CardState {
    pub hovered: bool,
    pub active: bool,
    pub attention: bool,
}

fn card_fill(pal: &Palette, st: CardState) -> (Color32, Color32) {
    if st.hovered {
        (pal.hover_bg, pal.surface_end)
    } else {
        (pal.surface, pal.surface_end)
    }
}

/// Paints everything behind a card's content: shadows, glows, fill, border, highlights.
/// `status_tint` is a status wash that replaces the surface gradient (attention, failed).
pub fn paint_card(
    painter: &Painter,
    id: Id,
    rect: Rect,
    pal: &Palette,
    st: CardState,
    status_tint: Option<Color32>,
) {
    let lk = look();
    let c = lk.card;
    let r = f32::from(c.radius);
    let cr = CornerRadius::same(c.radius);
    let (fill_a, fill_b) = match status_tint {
        Some(t) => {
            let t0 = if st.hovered {
                mix(t, pal.hover_bg, 0.6)
            } else {
                t
            };
            (t0, mix(t0, pal.surface_end, 0.5))
        }
        None => card_fill(pal, st),
    };
    let lifted = st.hovered && c.hover == Hover::Hard;

    // Outside: shadows, glows, rings.
    match c.shadow {
        super::CardShadow::None => {}
        super::CardShadow::Soft { dy, blur, alpha: a } => {
            let extra = if st.hovered && matches!(c.hover, Hover::Lift | Hover::Halo) {
                1.4
            } else {
                1.0
            };
            painter.add(
                Shadow {
                    offset: [0, dy],
                    blur,
                    spread: 0,
                    color: black((f32::from(a) * extra).min(255.0) as u8),
                }
                .as_shape(rect, cr),
            );
        }
        super::CardShadow::Hard => {
            let (off, col) = if st.active {
                (5, alpha(pal.accent, 140))
            } else if st.attention {
                (4, alpha(pal.warn, 115))
            } else if lifted {
                (6, white(51))
            } else {
                (4, black(255))
            };
            painter.rect_filled(rect.translate(vec2(off as f32, off as f32)), cr, col);
        }
    }
    if st.hovered && c.hover == Hover::Glow {
        painter.add(
            Shadow {
                offset: [0, 0],
                blur: 28,
                spread: 0,
                color: alpha(pal.accent, 77),
            }
            .as_shape(rect, cr),
        );
    }
    if st.hovered && c.hover == Hover::Halo {
        painter.rect_stroke(
            rect.expand(1.0),
            c.radius + 1,
            Stroke::new(1.0, white(64)),
            StrokeKind::Inside,
        );
    }
    if st.hovered && c.hover == Hover::Ring2 {
        painter.rect_stroke(
            rect.expand(1.0),
            c.radius + 1,
            Stroke::new(1.0, pal.hover_line),
            StrokeKind::Inside,
        );
    }
    if st.active {
        match c.active {
            Active::Ring { ring, alpha: a } => {
                painter.rect_stroke(
                    rect.expand(f32::from(ring) / 2.0),
                    c.radius + ring / 2,
                    Stroke::new(f32::from(ring), alpha(pal.accent, a)),
                    StrokeKind::Middle,
                );
            }
            Active::Grad => {
                painter.add(
                    Shadow {
                        offset: [0, 14],
                        blur: 40,
                        spread: 0,
                        color: Color32::from_rgba_unmultiplied(124, 58, 237, 46),
                    }
                    .as_shape(rect, cr),
                );
                painter.rect_stroke(
                    rect.expand(2.0),
                    c.radius + 2,
                    Stroke::new(4.0, alpha(pal.accent, 26)),
                    StrokeKind::Middle,
                );
            }
            Active::TopEdge => {
                painter.add(
                    Shadow {
                        offset: [0, 12],
                        blur: 36,
                        spread: 0,
                        color: alpha(pal.accent, 31),
                    }
                    .as_shape(rect, cr),
                );
            }
            Active::Glow { blur, alpha: a } => {
                painter.add(
                    Shadow {
                        offset: [0, 0],
                        blur,
                        spread: 0,
                        color: alpha(pal.accent, a),
                    }
                    .as_shape(rect, cr),
                );
            }
            Active::Spot => {
                painter.add(
                    Shadow {
                        offset: [0, 0],
                        blur: 60,
                        spread: 0,
                        color: alpha(pal.accent, 33),
                    }
                    .as_shape(rect, cr),
                );
            }
            Active::Phosphor => {
                painter.add(
                    Shadow {
                        offset: [0, 0],
                        blur: 18,
                        spread: 0,
                        color: alpha(pal.accent, 46),
                    }
                    .as_shape(rect, cr),
                );
            }
            Active::Border | Active::Hard | Active::Double => {}
        }
    }
    if c.double_rule {
        let col = if st.active {
            alpha(pal.accent, 89)
        } else if st.hovered {
            alpha(pal.hover_line, 140)
        } else {
            alpha(pal.stroke, 153)
        };
        painter.rect_stroke(
            rect.expand(3.0),
            c.radius + 3,
            Stroke::new(1.0, col),
            StrokeKind::Outside,
        );
    }

    // Border.
    let border = if st.active {
        pal.accent
    } else if st.attention {
        alpha(pal.warn, 166)
    } else if st.hovered {
        pal.hover_line
    } else {
        pal.stroke
    };
    let grad_border =
        (st.active && c.active == Active::Grad) || (st.hovered && c.hover == Hover::Grad);
    let bw = c.border_w;
    if grad_border {
        let (g0, g1) = if st.active {
            (pal.accent_a, pal.accent_b)
        } else {
            (alpha(pal.accent_a, 191), alpha(pal.accent_b, 140))
        };
        gradient_card(painter, id.with("gb"), rect, r, (g0, g1), Dir::Diagonal);
        gradient_card(
            painter,
            id.with("gf"),
            rect.shrink(bw),
            (r - bw).max(0.0),
            (fill_a, fill_b),
            Dir::Down,
        );
    } else {
        gradient_card(painter, id, rect, r, (fill_a, fill_b), Dir::Down);
        painter.rect_stroke(rect, cr, Stroke::new(bw, border), StrokeKind::Inside);
    }

    // Inside: highlights and edges.
    top_highlight(painter, rect, r, c.hi);
    if st.active {
        match c.active {
            Active::TopEdge => {
                painter.rect_filled(
                    Rect::from_min_size(
                        pos2(rect.left() + r * 0.6, rect.top() + 1.0),
                        vec2(rect.width() - r * 1.2, 2.0),
                    ),
                    1,
                    pal.accent,
                );
            }
            Active::Phosphor => {
                painter.rect_stroke(
                    rect.shrink(1.0),
                    c.radius.saturating_sub(1),
                    Stroke::new(1.0, alpha(pal.accent, 64)),
                    StrokeKind::Inside,
                );
            }
            _ => {}
        }
    }
}

/// Header band of a two-tone card (Two-Tone Frame): the top `band_h` px, with a bottom rule.
pub fn paint_card_band(painter: &Painter, rect: Rect, band_h: f32, pal: &Palette, hovered: bool) {
    let c = look().card;
    if !c.two_tone {
        return;
    }
    let r = c.radius.saturating_sub(1);
    let band = Rect::from_min_size(
        rect.min + vec2(1.0, 1.0),
        vec2(rect.width() - 2.0, band_h - 1.0),
    );
    let fill = if hovered {
        rgb_of(0x2a2a31)
    } else {
        rgb_of(0x222228)
    };
    painter.rect_filled(
        band,
        CornerRadius {
            nw: r,
            ne: r,
            sw: 0,
            se: 0,
        },
        fill,
    );
    let rule = if hovered { pal.hover_line } else { pal.stroke };
    painter.hline(band.x_range(), band.bottom() + 0.5, Stroke::new(1.0, rule));
}

/// Opacity of a card that is neither active nor hovered.
pub fn card_opacity(st: CardState) -> f32 {
    if st.active || st.hovered {
        1.0
    } else {
        look().card.idle
    }
}

// ------------------------------------------------------------------ tabs

/// Hover / active state of a tab (animated 0..=1).
#[derive(Clone, Copy)]
pub struct TabState {
    pub hov: f32,
    pub sel: f32,
    /// Shown in another pane of the split without having the focus.
    pub shown: bool,
    pub dark: bool,
}

/// Corner radius of the signal-edge tabs: the style's tab radius, kept tight (2..=6 px) so the
/// slanted corners stay crisp.
pub fn tab_radius() -> f32 {
    f32::from(look().tab_r.clamp(2, 6))
}

/// Horizontal room the slant takes at each end of a tab pill `h` px tall: the outline meets the
/// pill's left edge at the bottom and its right edge at the top.
pub fn tab_slant(h: f32) -> f32 {
    h * crate::deco::SLANT_PER_PX / 2.0
}

/// Paints the signal-edge tab inside `pill` (a slanted parallelogram that never leaves the pill's
/// width): resting outline, hover plate, and for the active tab a dark plate with the accent
/// gradient outline, glow and drop shadow, plus the style's own touch. Returns the outline, for
/// overlays such as the attention wash.
pub fn paint_tab(painter: &Painter, pill: Rect, pal: &Palette, st: TabState) -> Vec<Pos2> {
    use crate::deco;
    let kind = look().tab;
    let TabState {
        hov,
        sel,
        shown,
        dark,
    } = st;
    let r = tab_radius();
    let s = tab_slant(pill.height());
    // The parallelogram's central rectangle: shadows and glows hug it.
    let body = pill.shrink2(vec2(s, 0.0));
    let pts = deco::slanted(body, r);
    let brutal = kind == TabKind::Brutal;
    let w = if brutal { 2.0 } else { 1.0 };
    // How much of the accent edge shows: fully when active, dimmed for a split pane's tab.
    let lit = if shown { sel + (1.0 - sel) * 0.45 } else { sel };
    let rest_edge = 1.0 - if shown { 1.0 } else { sel };
    let plate = mix(pal.bar, pal.surface, 0.5);

    // Depth under the active tab.
    if sel > 0.01 {
        if brutal {
            deco::fill(
                painter,
                pts.iter().map(|p| *p + vec2(3.0, 3.0)).collect(),
                pal.accent.gamma_multiply(sel),
            );
        } else {
            let ri = r as u8;
            if kind != TabKind::Glass {
                deco::drop_shadow(painter, body, ri, 6, 14, dark, (153.0 * sel) as u8);
            }
            // Halo over the shadow: a tight core and a wide bloom.
            let k = sel * if dark { 1.0 } else { 0.5 };
            let (a, _) = deco::accent_pair(pal);
            deco::glow(painter, body, ri, a.gamma_multiply(0.18 * k), 22);
            deco::glow(painter, body, ri, a.gamma_multiply(0.22 * k), 8);
        }
    }

    // Hover plate, fading out as the tab becomes active.
    deco::fill(
        painter,
        pts.clone(),
        pal.hover_bg.gamma_multiply(hov * (1.0 - sel)),
    );
    // Split pane's tab: a faint plate.
    if shown {
        deco::fill(
            painter,
            pts.clone(),
            plate.gamma_multiply(0.5 * (1.0 - sel)),
        );
    }
    // Active plate.
    if sel > 0.01 {
        let fill = match kind {
            TabKind::Filled => pal.panel,
            TabKind::Tint => mix(plate, pal.accent, 0.12),
            TabKind::Glass => {
                // The bar under the glass, so the glow stays outside the tab.
                deco::fill(painter, pts.clone(), pal.bar.gamma_multiply(sel));
                white(20)
            }
            TabKind::Folder => pal.bg,
            TabKind::Plain | TabKind::Outlined | TabKind::Grad | TabKind::Brutal => plate,
        };
        deco::fill(painter, pts.clone(), fill.gamma_multiply(sel));
        let hi = match kind {
            TabKind::Glass => 31.0,
            TabKind::Filled => 20.0,
            _ => 0.0,
        };
        if hi > 0.0 {
            // 1 px highlight along the slanted top edge (it runs from `left + 2s` to `right`).
            painter.hline(
                (body.left() + s + r)..=(body.right() + s - r),
                body.top() + 1.0,
                Stroke::new(1.0, white((hi * sel) as u8)),
            );
        }
    }

    // Resting / hover outline.
    if rest_edge > 0.01 {
        // Clearly visible at rest (light strokes sit close to the bar: full strength there).
        let rest = match kind {
            TabKind::Outlined | TabKind::Brutal => pal.stroke,
            _ => mix(pal.bar, pal.stroke, if dark { 0.8 } else { 1.0 }),
        };
        let hover = mix(pal.stroke, pal.hover_line, 0.35);
        deco::outline(
            painter,
            pts.clone(),
            Stroke::new(w, mix(rest, hover, hov).gamma_multiply(rest_edge)),
        );
    }
    // Accent gradient outline.
    if lit > 0.01 {
        let (a, b) = deco::accent_pair(pal);
        deco::gradient_outline(
            painter,
            pts.clone(),
            w,
            &[(0.0, a.gamma_multiply(lit)), (1.0, b.gamma_multiply(lit))],
            vec2(1.0, 0.0),
        );
    }
    pts
}

// ------------------------------------------------------------------ chips and fields

/// A chip fill: raised and borderless normally, outlined and transparent where the style asks.
pub fn paint_chip(painter: &Painter, rect: Rect, radius: u8, pal: &Palette, fill: Color32) {
    if look().outlined_chips {
        painter.rect_stroke(
            rect,
            radius.min(look().ui_r.max(2)),
            Stroke::new(1.0, mix(pal.stroke, pal.text, 0.15)),
            StrokeKind::Inside,
        );
    } else {
        painter.rect_filled(rect, radius, fill);
    }
}

/// Inner top highlight of fields and pills (Obsidian Glass).
pub fn paint_field_highlight(painter: &Painter, rect: Rect, radius: f32) {
    if look().glass_fields {
        top_highlight(painter, rect, radius, 15);
    }
}

/// A violet→cyan horizontal gradient pill (Aurora's "All" pill border, Agents chip border).
pub fn gradient_outline(
    painter: &Painter,
    id: Id,
    rect: Rect,
    radius: f32,
    pal: &Palette,
    fill: Color32,
) {
    gradient_card(
        painter,
        id.with("go"),
        rect,
        radius,
        (pal.accent_a, pal.accent_b),
        Dir::Right,
    );
    painter.rect_filled(rect.shrink(1.0), (radius - 1.0).max(0.0) as u8, fill);
}
