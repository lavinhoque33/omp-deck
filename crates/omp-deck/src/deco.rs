//! Depth and edge primitives of the "signal edge" chrome (tabs, gradient-rule cards) and the
//! "drafted" buttons: accent gradient pairs, rounded convex outlines (rectangles and slanted
//! parallelograms), gradient strokes, gradient fills, glows and hard base shadows.
//!
//! Everything paints through a [`Painter`]; nothing here keeps state.

use std::sync::Arc;

use egui::epaint::{ColorMode, PathShape, PathStroke};
use egui::{Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Shadow, Shape, Stroke, Vec2, vec2};

use crate::theme::{self, Palette};

/// Slant of the signal-edge tabs: the top edge leans right by `tan(12°)` per pixel of height.
pub const SLANT_PER_PX: f32 = 0.212_556_56;

/// The two ends of the style's accent gradient: `accent_a → accent_b`, or, for single-accent
/// styles, the accent and a softer partner: hue turned 45°, saturation ×0.6, value ≥ 0.95
/// (Blueprint cyan → lavender `#818cf8`-like, teal → periwinkle, pink → coral, green → aqua).
pub fn accent_pair(p: &Palette) -> (Color32, Color32) {
    let a = p.accent_a;
    if p.accent_b != a {
        (a, p.accent_b)
    } else {
        let mut h = egui::ecolor::Hsva::from(hue_shift(a, 45.0));
        h.s *= 0.6;
        h.v = h.v.max(0.95);
        (a, Color32::from(h))
    }
}

/// `c` with its hue turned by `deg` degrees (saturation, value and alpha kept).
pub fn hue_shift(c: Color32, deg: f32) -> Color32 {
    let mut h = egui::ecolor::Hsva::from(c);
    h.h = (h.h + deg / 360.0).rem_euclid(1.0);
    Color32::from(h)
}

/// Gradient pair of a status tone: the accent pair for `Color32::TRANSPARENT`, else `tone` and its
/// 30°-warmer neighbour (danger red → pink, warn amber → orange).
pub fn tone_pair(p: &Palette, tone: Color32) -> (Color32, Color32) {
    if tone == Color32::TRANSPARENT {
        accent_pair(p)
    } else {
        (tone, hue_shift(tone, -30.0))
    }
}

/// Outline of the convex polygon `corners` (clockwise or counter-clockwise) with every corner
/// filleted by `radius` (clamped so neighbouring fillets never overlap).
pub fn rounded_polygon(corners: &[Pos2], radius: f32) -> Vec<Pos2> {
    const STEPS: usize = 5;
    let n = corners.len();
    if radius <= 0.25 || n < 3 {
        return corners.to_vec();
    }
    let mut out = Vec::with_capacity(n * (STEPS + 1));
    for i in 0..n {
        let p = corners[i];
        let a = corners[(i + n - 1) % n];
        let b = corners[(i + 1) % n];
        let (la, lb) = ((a - p).length(), (b - p).length());
        if la < 1e-3 || lb < 1e-3 {
            out.push(p);
            continue;
        }
        let (u, v) = ((a - p) / la, (b - p) / lb);
        let half = (u.dot(v).clamp(-1.0, 1.0)).acos() / 2.0;
        if half < 1e-3 || half > std::f32::consts::FRAC_PI_2 - 1e-3 {
            out.push(p);
            continue;
        }
        // Tangent distance of the fillet, clamped to half of each adjacent edge.
        let d = (radius / half.tan()).min(la / 2.0).min(lb / 2.0);
        let r = d * half.tan();
        let center = p + (u + v).normalized() * (r / half.sin());
        let (s, e) = (p + u * d, p + v * d);
        let (a0, a1) = ((s - center).angle(), (e - center).angle());
        let mut sweep = a1 - a0;
        if sweep > std::f32::consts::PI {
            sweep -= std::f32::consts::TAU;
        } else if sweep < -std::f32::consts::PI {
            sweep += std::f32::consts::TAU;
        }
        for k in 0..=STEPS {
            let t = a0 + sweep * k as f32 / STEPS as f32;
            out.push(center + r * Vec2::angled(t));
        }
    }
    out
}

/// Rounded rectangle outline (clockwise from the top-left).
pub fn rounded_rect(rect: Rect, radius: f32) -> Vec<Pos2> {
    rounded_polygon(
        &[
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ],
        radius,
    )
}

/// Slanted tab outline inside `rect`: a parallelogram whose top edge is shifted right of its
/// bottom edge by `rect.height() * SLANT_PER_PX`, centred on `rect`, corners filleted by `radius`.
pub fn slanted(rect: Rect, radius: f32) -> Vec<Pos2> {
    let s = rect.height() * SLANT_PER_PX / 2.0;
    rounded_polygon(
        &[
            rect.left_top() + vec2(s, 0.0),
            rect.right_top() + vec2(s, 0.0),
            rect.right_bottom() - vec2(s, 0.0),
            rect.left_bottom() - vec2(s, 0.0),
        ],
        radius,
    )
}

/// Fills the convex outline `pts` with one colour.
pub fn fill(painter: &Painter, pts: Vec<Pos2>, color: Color32) {
    if color.a() > 0 {
        painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
    }
}

/// Fills the convex outline `pts` with a vertical gradient `top → bottom` (triangle fan).
pub fn fill_vertical(painter: &Painter, pts: &[Pos2], top: Color32, bottom: Color32) {
    if top == bottom {
        fill(painter, pts.to_vec(), top);
        return;
    }
    let Some(bounds) = bounds(pts) else { return };
    let col = |p: Pos2| {
        top.lerp_to_gamma(
            bottom,
            ((p.y - bounds.min.y) / bounds.height().max(1.0)).clamp(0.0, 1.0),
        )
    };
    let c = bounds.center();
    let mut m = Mesh::default();
    m.colored_vertex(c, col(c));
    for p in pts {
        m.colored_vertex(*p, col(*p));
    }
    let n = pts.len() as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    painter.add(Shape::mesh(m));
}

/// Closed outline of width `width` drawn inside `pts`, coloured by a linear gradient across the
/// outline's bounding box along `dir` (`(1,0)` left → right, `(1,1)` top-left → bottom-right).
/// `stops` are `(position 0..=1, colour)` in ascending order.
pub fn gradient_outline(
    painter: &Painter,
    pts: Vec<Pos2>,
    width: f32,
    stops: &[(f32, Color32)],
    dir: Vec2,
) {
    painter.add(gradient_outline_shape(pts, width, stops, dir));
}

/// The [`gradient_outline`] shape, for painting into a reserved slot ([`Painter::set`]).
pub fn gradient_outline_shape(
    pts: Vec<Pos2>,
    width: f32,
    stops: &[(f32, Color32)],
    dir: Vec2,
) -> Shape {
    let stops: Arc<[(f32, Color32)]> = stops.into();
    let dir = dir.normalized();
    let color = ColorMode::UV(Arc::new(move |bb: Rect, p: Pos2| {
        let span = (bb.width() * dir.x.abs() + bb.height() * dir.y.abs()).max(1.0);
        let origin = pos2_min_along(bb, dir);
        let t = ((p - origin).dot(dir) / span).clamp(0.0, 1.0);
        sample(&stops, t)
    }));
    Shape::Path(PathShape {
        points: pts,
        closed: true,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke {
            width,
            color,
            kind: egui::StrokeKind::Inside,
        },
    })
}

/// Closed outline of one colour.
pub fn outline(painter: &Painter, pts: Vec<Pos2>, stroke: Stroke) {
    if stroke.width > 0.0 && stroke.color.a() > 0 {
        painter.add(Shape::Path(PathShape {
            points: pts,
            closed: true,
            fill: Color32::TRANSPARENT,
            stroke: PathStroke {
                width: stroke.width,
                color: ColorMode::Solid(stroke.color),
                kind: egui::StrokeKind::Inside,
            },
        }));
    }
}

/// Horizontal 1 px rule from `a` to `b` coloured by `stops` (fades, accent sweeps).
pub fn gradient_rule(painter: &Painter, a: Pos2, b: Pos2, width: f32, stops: &[(f32, Color32)]) {
    painter.add(gradient_rule_shape(a, b, width, stops));
}

/// The [`gradient_rule`] shape, for painting into a reserved slot ([`Painter::set`]).
pub fn gradient_rule_shape(a: Pos2, b: Pos2, width: f32, stops: &[(f32, Color32)]) -> Shape {
    let len = (b - a).length().max(1.0);
    let dir = (b - a) / len;
    let mut m = Mesh::default();
    let n = vec2(-dir.y, dir.x) * (width / 2.0);
    for &(t, c) in stops {
        let p = a + dir * (len * t);
        m.colored_vertex(p - n, c);
        m.colored_vertex(p + n, c);
    }
    for i in 0..stops.len().saturating_sub(1) as u32 {
        let k = i * 2;
        m.add_triangle(k, k + 1, k + 2);
        m.add_triangle(k + 1, k + 3, k + 2);
    }
    Shape::mesh(m)
}

/// Soft glow (zero-offset blurred shadow) of `color` around `rect`.
pub fn glow(painter: &Painter, rect: Rect, radius: u8, color: Color32, blur: u8) {
    if color.a() > 0 {
        let s = Shadow {
            offset: [0, 0],
            blur,
            spread: 0,
            color,
        };
        painter.add(s.as_shape(rect, CornerRadius::same(radius)));
    }
}

/// Soft drop shadow under `rect` (`dy` down, `blur` wide), stronger on dark themes.
pub fn drop_shadow(
    painter: &Painter,
    rect: Rect,
    radius: u8,
    dy: i8,
    blur: u8,
    dark: bool,
    alpha: u8,
) {
    let s = Shadow {
        offset: [0, dy],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(if dark { alpha } else { alpha / 3 }),
    };
    painter.add(s.as_shape(rect, CornerRadius::same(radius)));
}

/// Readable dark ink for glyphs on a gradient tile of `a → b`.
pub fn ink_on_pair(a: Color32, b: Color32) -> Color32 {
    theme::ink_on(theme::mix(a, b, 0.5))
}

/// Fills the convex outline `pts` with a linear gradient `a → b` across its bounding box along
/// `dir` (`(1,1)` = top-left → bottom-right), as a triangle fan.
pub fn fill_linear(painter: &Painter, pts: &[Pos2], a: Color32, b: Color32, dir: Vec2) {
    let Some(bb) = bounds(pts) else { return };
    let dir = dir.normalized();
    let span = (bb.width() * dir.x.abs() + bb.height() * dir.y.abs()).max(1.0);
    let origin = pos2_min_along(bb, dir);
    let col = |p: Pos2| a.lerp_to_gamma(b, ((p - origin).dot(dir) / span).clamp(0.0, 1.0));
    let c = bb.center();
    let mut m = Mesh::default();
    m.colored_vertex(c, col(c));
    for p in pts {
        m.colored_vertex(*p, col(*p));
    }
    let n = pts.len() as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    painter.add(Shape::mesh(m));
}

/// The [`glow`] shape (`Shape::Noop` when `color` is transparent).
pub fn glow_shape(rect: Rect, radius: u8, color: Color32, blur: u8) -> Shape {
    if color.a() == 0 {
        return Shape::Noop;
    }
    Shadow {
        offset: [0, 0],
        blur,
        spread: 0,
        color,
    }
    .as_shape(rect, CornerRadius::same(radius))
    .into()
}

/// The [`drop_shadow`] shape.
pub fn drop_shadow_shape(rect: Rect, radius: u8, dy: i8, blur: u8, dark: bool, alpha: u8) -> Shape {
    Shadow {
        offset: [0, dy],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(if dark { alpha } else { alpha / 3 }),
    }
    .as_shape(rect, CornerRadius::same(radius))
    .into()
}

/// Soft radial wash of `color` (its alpha is the peak) spreading from the top-left corner of
/// `rect` over an ellipse of half-axes `reach`, gone at 70 % of the ellipse; the top-left corner
/// is rounded by `radius` so the wash never spills outside a rounded card.
pub fn corner_wash(rect: Rect, radius: f32, reach: Vec2, color: Color32) -> Shape {
    const COLS: usize = 12;
    const ROWS: usize = 4;
    if color.a() == 0 || rect.width() < 1.0 || rect.height() < 1.0 {
        return Shape::Noop;
    }
    let r = radius.clamp(0.0, rect.width().min(rect.height()) / 2.0);
    let at = |p: Pos2| {
        let d = vec2((p.x - rect.min.x) / reach.x, (p.y - rect.min.y) / reach.y).length();
        color.gamma_multiply((1.0 - d / 0.7).clamp(0.0, 1.0))
    };
    // Columns / rows: the corner fillet's edge first, then even steps to the far side.
    let xs = |i: usize| -> f32 {
        if i == 0 {
            rect.min.x
        } else {
            rect.min.x + r + (rect.width() - r) * (i - 1) as f32 / (COLS - 1) as f32
        }
    };
    let ys = |j: usize| -> f32 {
        if j == 0 {
            rect.min.y
        } else {
            rect.min.y + r + (rect.height() - r) * (j - 1) as f32 / (ROWS - 1) as f32
        }
    };
    let mut m = Mesh::default();
    let w = COLS as u32 + 1;
    for j in 0..=ROWS {
        for i in 0..=COLS {
            let p = Pos2::new(xs(i), ys(j));
            m.colored_vertex(p, at(p));
        }
    }
    for j in 0..ROWS as u32 {
        for i in 0..COLS as u32 {
            if i == 0 && j == 0 && r > 0.5 {
                continue;
            }
            let k = j * w + i;
            m.add_triangle(k, k + 1, k + w);
            m.add_triangle(k + 1, k + w + 1, k + w);
        }
    }
    if r > 0.5 {
        // The corner cell becomes a fan around the fillet centre (grid vertex (1,1)).
        let c = w + 1;
        let center = Pos2::new(rect.min.x + r, rect.min.y + r);
        let mut prev = w; // grid vertex (0,1): the fillet's left end
        const STEPS: u32 = 6;
        for s in 1..STEPS {
            let a = std::f32::consts::PI + std::f32::consts::FRAC_PI_2 * s as f32 / STEPS as f32;
            let p = center + r * Vec2::angled(a);
            let idx = m.vertices.len() as u32;
            m.colored_vertex(p, at(p));
            m.add_triangle(c, prev, idx);
            prev = idx;
        }
        m.add_triangle(c, prev, 1); // grid vertex (1,0): the fillet's top end
    }
    Shape::mesh(m)
}

fn bounds(pts: &[Pos2]) -> Option<Rect> {
    let mut it = pts.iter();
    let first = *it.next()?;
    Some(it.fold(Rect::from_min_max(first, first), |r, p| {
        r.union(Rect::from_min_max(*p, *p))
    }))
}

/// Corner of `bb` where the projection on `dir` is smallest.
fn pos2_min_along(bb: Rect, dir: Vec2) -> Pos2 {
    Pos2::new(
        if dir.x >= 0.0 { bb.min.x } else { bb.max.x },
        if dir.y >= 0.0 { bb.min.y } else { bb.max.y },
    )
}

fn sample(stops: &[(f32, Color32)], t: f32) -> Color32 {
    let Some(&(t0, c0)) = stops.first() else {
        return Color32::TRANSPARENT;
    };
    if t <= t0 {
        return c0;
    }
    for w in stops.windows(2) {
        let ((ta, ca), (tb, cb)) = (w[0], w[1]);
        if t <= tb {
            return ca.lerp_to_gamma(cb, ((t - ta) / (tb - ta).max(1e-4)).clamp(0.0, 1.0));
        }
    }
    stops.last().map_or(c0, |s| s.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_accent_styles_get_a_distinct_partner() {
        let c = Color32::from_rgb(0x22, 0xd3, 0xee);
        let shifted = hue_shift(c, 45.0);
        assert_ne!(shifted, c);
        // Cyan turned 45° lands in blue/indigo: blue channel dominates.
        assert!(
            shifted.b() > shifted.r() && shifted.b() > shifted.g(),
            "{shifted:?}"
        );
        assert_eq!(hue_shift(c, 360.0), c);
    }

    #[test]
    fn fillets_stay_inside_short_edges() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(40.0, 6.0));
        let pts = rounded_rect(rect, 10.0);
        let bb = bounds(&pts).unwrap_or(Rect::NOTHING);
        assert!(bb.min.x >= -0.01 && bb.max.x <= 40.01, "{bb:?}");
        assert!(bb.min.y >= -0.01 && bb.max.y <= 6.01, "{bb:?}");
    }

    #[test]
    fn slanted_top_leans_right_and_stays_centred() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(200.0, 32.0));
        let pts = slanted(rect, 0.0);
        let s = 32.0 * SLANT_PER_PX / 2.0;
        assert_eq!(pts[0], Pos2::new(s, 0.0));
        assert_eq!(pts[3], Pos2::new(-s, 32.0));
    }

    #[test]
    fn gradient_stops_interpolate_and_clamp() {
        let stops = [(0.0, Color32::BLACK), (1.0, Color32::WHITE)];
        assert_eq!(sample(&stops, -1.0), Color32::BLACK);
        assert_eq!(sample(&stops, 2.0), Color32::WHITE);
        let mid = sample(&stops, 0.5);
        assert!(mid.r() > 100 && mid.r() < 160, "{mid:?}");
    }
}
