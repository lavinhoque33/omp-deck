//! Grid painting (docs/notes/egui-fonts.md §Recommended strategy).
//!
//! Per frame: [`Frame::capture`] copies the visible cells out of the locked `Term` into reusable
//! buffers (colors already resolved), the lock is released, then [`paint`] draws:
//! one background mesh (merged runs), one `LayoutJob` per row (style-run sections; cells not in
//! the primary face at exactly one cell are blanked and drawn as per-cell galleys at their
//! column), and one foreground mesh (procedural box/block/braille/powerline glyphs, underline
//! variants, strikethrough, non-block cursor shapes).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::{Flags, Hyperlink};
use alacritty_terminal::term::{Term, TermMode, point_to_viewport};
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor, Rgb};
use egui::epaint::text::{ByteIndex, Galley, LayoutJob, LayoutSection, TextFormat};
use egui::epaint::{Color32, Mesh, Pos2, Rect, Shape, Stroke, pos2, vec2};
use egui::{FontFamily, FontId, Painter};

use crate::core::Listener;
use crate::fonts::{FAMILY_BOLD, FAMILY_PRIMARY, FAMILY_TERM};
use crate::graphics::{self, Graphics, PLACEHOLDER, Placeholder};
use crate::theme::{self, Palette};

pub(crate) fn c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

/// Integer-pixel cell metrics for one (font size, pixels_per_point).
#[derive(Clone, Debug)]
pub(crate) struct Metrics {
    pub ppp: f32,
    pub font_size: f32,
    pub cell_w_px: f32,
    pub cell_h_px: f32,
    /// Cell size in points (= px / ppp).
    pub cell_w: f32,
    pub cell_h: f32,
    /// `extra_letter_spacing` that turns the font advance into exactly `cell_w`.
    pub spacing: f32,
    /// Font advance of 'M' in points.
    pub advance: f32,
    pub regular: FontFamily,
    pub bold: FontFamily,
    pub primary: FontFamily,
}

impl Metrics {
    pub fn compute(ctx: &egui::Context, font_size: f32) -> Metrics {
        let ppp = ctx.pixels_per_point();
        let installed = ctx.fonts(|f| {
            f.definitions()
                .families
                .contains_key(&FontFamily::Name(FAMILY_TERM.into()))
        });
        let (regular, bold, primary) = if installed {
            (
                FontFamily::Name(FAMILY_TERM.into()),
                FontFamily::Name(FAMILY_BOLD.into()),
                FontFamily::Name(FAMILY_PRIMARY.into()),
            )
        } else {
            (
                FontFamily::Monospace,
                FontFamily::Monospace,
                FontFamily::Monospace,
            )
        };
        let id = FontId::new(font_size, regular.clone());
        let (advance, row_h) = ctx.fonts_mut(|f| (f.glyph_width(&id, 'M'), f.row_height(&id)));
        let adv_px = advance * ppp;
        let cell_w_px = adv_px.round().max(1.0);
        let cell_h_px = (row_h * ppp).ceil().max(1.0);
        Metrics {
            ppp,
            font_size,
            cell_w_px,
            cell_h_px,
            cell_w: cell_w_px / ppp,
            cell_h: cell_h_px / ppp,
            spacing: (cell_w_px - adv_px) / ppp,
            advance,
            regular,
            bold,
            primary,
        }
    }
}

/// Upper bound on cached per-cell galleys before the cache is rebuilt from scratch.
const MAX_SPECIAL_GALLEYS: usize = 4096;

/// Identity of a per-cell galley: base char, combining marks (hashed; verified against the
/// galley text on lookup) and face. Color is not part of it: galleys are laid out with
/// `Color32::PLACEHOLDER` and tinted at paint time.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SpecialKey {
    c: char,
    zw_hash: u64,
    zw_len: u16,
    bold: bool,
    italic: bool,
}

/// Per-terminal glyph caches: per char whether it can stay in the row job (primary face,
/// one-cell advance), and the galleys of per-cell glyphs (fallback faces, wide chars,
/// combining clusters) so CJK-heavy screens cost a hash lookup per cell, not a layout job.
#[derive(Default)]
pub(crate) struct GlyphCache {
    fits: HashMap<char, bool>,
    galleys: HashMap<SpecialKey, Arc<Galley>>,
    /// (ppp, font size) the caches were built for.
    metrics_key: Option<(u32, u32)>,
    /// Galley of a fixed job, re-requested each frame: egui returns the same `Arc` until it
    /// clears its galley cache (font atlas reset, text options or font change), which is
    /// exactly when the cached galleys' atlas coordinates go stale.
    probe: Option<Arc<Galley>>,
}

impl GlyphCache {
    /// Call once per paint, before any [`Self::special`].
    fn begin_frame(&mut self, painter: &Painter, m: &Metrics) {
        let key = (m.ppp.to_bits(), m.font_size.to_bits());
        if self.metrics_key != Some(key) {
            self.metrics_key = Some(key);
            self.fits.clear();
            self.galleys.clear();
        }
        let probe = painter.layout_job(LayoutJob::single_section(
            "M".to_owned(),
            TextFormat::simple(
                FontId::new(m.font_size, m.regular.clone()),
                Color32::PLACEHOLDER,
            ),
        ));
        let same = self.probe.as_ref().is_some_and(|p| Arc::ptr_eq(p, &probe));
        if !same {
            // Fonts may have changed too: re-classify chars.
            self.fits.clear();
            self.galleys.clear();
        } else if self.galleys.len() > MAX_SPECIAL_GALLEYS {
            self.galleys.clear();
        }
        self.probe = Some(probe);
    }

    fn fits_row(&mut self, ctx: &egui::Context, m: &Metrics, c: char) -> bool {
        if c.is_ascii() {
            return true;
        }
        if let Some(&f) = self.fits.get(&c) {
            return f;
        }
        let primary = FontId::new(m.font_size, m.primary.clone());
        let fits = ctx.fonts_mut(|f| {
            f.has_glyph(&primary, c)
                && (f.glyph_width(&primary, c) - m.advance).abs() <= m.advance * 0.02
        });
        self.fits.insert(c, fits);
        fits
    }

    /// Galley for `c` + combining `zw`; laid out only on a cache miss.
    fn special(
        &mut self,
        painter: &Painter,
        font: &FontId,
        m: &Metrics,
        c: char,
        zw: &[char],
        (bold, italic): (bool, bool),
    ) -> Arc<Galley> {
        let mut h = DefaultHasher::new();
        zw.hash(&mut h);
        let key = SpecialKey {
            c,
            zw_hash: h.finish(),
            zw_len: zw.len().min(usize::from(u16::MAX)) as u16,
            bold,
            italic,
        };
        if let Some(g) = self.galleys.get(&key)
            && g.text()
                .chars()
                .eq(std::iter::once(c).chain(zw.iter().copied()))
        {
            return g.clone();
        }
        let mut text = String::with_capacity(4 * (1 + zw.len()));
        text.push(c);
        text.extend(zw);
        let format = TextFormat {
            font_id: font.clone(),
            color: Color32::PLACEHOLDER,
            italics: italic,
            line_height: Some(m.cell_h),
            expand_bg: 0.0,
            ..TextFormat::default()
        };
        let galley = painter.layout_job(LayoutJob::single_section(text, format));
        self.galleys.insert(key, galley.clone());
        galley
    }
}

/// One captured cell; colors resolved (inverse/hidden/dim/selection applied).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FCell {
    pub c: char,
    pub fg: Color32,
    pub bg: Color32,
    pub underline: Color32,
    pub flags: Flags,
    /// Range into `Frame::zerowidth`.
    pub zw_start: u32,
    pub zw_len: u16,
    /// 0 = none, else index + 1 into `Frame::links`.
    pub link: u16,
}

impl Default for FCell {
    fn default() -> Self {
        FCell {
            c: ' ',
            fg: Color32::WHITE,
            bg: Color32::BLACK,
            underline: Color32::WHITE,
            flags: Flags::empty(),
            zw_start: 0,
            zw_len: 0,
            link: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameCursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
    pub wide: bool,
}

/// Reusable snapshot of the visible grid.
#[derive(Default)]
pub(crate) struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<FCell>,
    pub zerowidth: Vec<char>,
    pub links: Vec<Hyperlink>,
    pub cursor: Option<FrameCursor>,
    pub cursor_color: Color32,
    pub blinking: bool,
    pub mode: TermMode,
    pub display_offset: usize,
    pub bg: Color32,
    /// Kitty graphics placeholder cells (drawn blank, image painted over them).
    pub placeholders: Vec<Placeholder>,
}

impl Frame {
    pub fn capture(&mut self, term: &Term<Listener>, palette: &Palette) {
        let cols = term.columns();
        let rows = term.screen_lines();
        let content = term.renderable_content();
        let colors = content.colors;
        let offset = content.display_offset;
        self.cols = cols;
        self.rows = rows;
        self.cells.clear();
        self.cells.resize(cols * rows, FCell::default());
        self.zerowidth.clear();
        self.links.clear();
        self.placeholders.clear();
        self.mode = content.mode;
        self.display_offset = offset;
        self.bg = c32(theme::indexed(
            colors,
            palette,
            NamedColor::Background as usize,
        ));
        self.cursor_color = c32(theme::indexed(colors, palette, NamedColor::Cursor as usize));
        let selection_bg = c32(palette.selection_bg);
        let selection = content.selection;

        for indexed in content.display_iter {
            let line = indexed.point.line.0 + offset as i32;
            if line < 0 || line as usize >= rows {
                continue;
            }
            let (row, col) = (line as usize, indexed.point.column.0);
            if col >= cols {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            let mut fg = theme::resolve(cell.fg, colors, palette, flags.contains(Flags::DIM));
            let mut bg = theme::resolve(cell.bg, colors, palette, false);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let underline = match cell.underline_color() {
                Some(u) => c32(theme::resolve(
                    u,
                    colors,
                    palette,
                    flags.contains(Flags::DIM),
                )),
                None => c32(fg),
            };
            let mut out = FCell {
                c: cell.c,
                fg: c32(fg),
                bg: c32(bg),
                underline,
                flags,
                zw_start: 0,
                zw_len: 0,
                link: 0,
            };
            if flags.contains(Flags::HIDDEN) {
                out.fg = out.bg;
                out.underline = out.bg;
                out.c = ' ';
            }
            if selection.is_some_and(|s| s.contains(indexed.point)) {
                out.bg = selection_bg;
            }
            if cell.c == PLACEHOLDER {
                // Raw colors carry the image/placement ids; the glyph itself is never drawn.
                out.c = ' ';
                let zw = cell.zerowidth().unwrap_or(&[]);
                let left = self.placeholders.last();
                let ul = cell.underline_color();
                if let Some(ph) = graphics::placeholder(row, col, cell.fg, ul, zw, left) {
                    self.placeholders.push(ph);
                }
            } else if cell.extra.is_some() {
                if let Some(zw) = cell.zerowidth() {
                    out.zw_start = self.zerowidth.len() as u32;
                    out.zw_len = zw.len().min(usize::from(u16::MAX)) as u16;
                    self.zerowidth
                        .extend_from_slice(&zw[..usize::from(out.zw_len)]);
                }
                if let Some(link) = cell.hyperlink() {
                    let idx = match self.links.iter().rposition(|l| *l == link) {
                        Some(i) => i,
                        None => {
                            self.links.push(link);
                            self.links.len() - 1
                        }
                    };
                    out.link = u16::try_from(idx + 1).unwrap_or(0);
                }
            }
            self.cells[row * cols + col] = out;
        }

        let cursor = content.cursor;
        self.cursor = match cursor.shape {
            CursorShape::Hidden => None,
            shape => point_to_viewport(offset, cursor.point).and_then(|p| {
                (p.line < rows && p.column.0 < cols).then(|| FrameCursor {
                    row: p.line,
                    col: p.column.0,
                    shape,
                    wide: self.cells[p.line * cols + p.column.0]
                        .flags
                        .contains(Flags::WIDE_CHAR),
                })
            }),
        };
        self.blinking = term.cursor_style().blinking;
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&FCell> {
        (row < self.rows && col < self.cols).then(|| &self.cells[row * self.cols + col])
    }
}

/// Viewport cells of a hovered plain-text URL, (row, col) inclusive; `start` may lie above the
/// viewport (negative row) when the URL's line is scrolled partly out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UrlSpan {
    pub start: (i32, usize),
    pub end: (i32, usize),
}

impl UrlSpan {
    fn contains(&self, row: usize, col: usize) -> bool {
        let p = (row as i32, col);
        self.start <= p && p <= self.end
    }
}

/// Inputs to [`paint`] besides the frame.
pub(crate) struct PaintParams<'a> {
    /// Pixel-snapped top-left of cell (0, 0).
    pub origin: Pos2,
    pub metrics: &'a Metrics,
    /// Cursor drawn this frame (blink phase on, cursor present).
    pub cursor_visible: bool,
    pub focused: bool,
    /// Hovered hyperlink (Ctrl held) to underline.
    pub hover_link: u16,
    /// Hovered plain-text URL (Ctrl held) to underline.
    pub hover_url: Option<UrlSpan>,
    /// Kitty graphics images shown by placeholder cells.
    pub graphics: &'a Graphics,
    pub preedit: &'a str,
    pub banner: Option<&'a str>,
}

/// Reusable scratch for [`paint`].
#[derive(Default)]
pub(crate) struct PaintScratch {
    specials: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    fg: Color32,
    bold: bool,
    italic: bool,
}

pub(crate) fn paint(
    frame: &Frame,
    painter: &Painter,
    glyphs: &mut GlyphCache,
    scratch: &mut PaintScratch,
    p: &PaintParams<'_>,
) {
    let m = p.metrics;
    let ctx = painter.ctx();
    glyphs.begin_frame(painter, m);
    let (cols, rows) = (frame.cols, frame.rows);
    let ox_px = (p.origin.x * m.ppp).round();
    let oy_px = (p.origin.y * m.ppp).round();
    let x_of = |col: usize| (ox_px + col as f32 * m.cell_w_px) / m.ppp;
    let y_of = |row: usize| (oy_px + row as f32 * m.cell_h_px) / m.ppp;

    let cursor = frame.cursor.filter(|_| p.cursor_visible);
    let block_cursor = cursor.filter(|c| p.focused && c.shape == CursorShape::Block);

    // 1. Backgrounds: merged runs of non-default bg, plus the block cursor.
    let mut bg_mesh = Mesh::default();
    for row in 0..rows {
        let cells = &frame.cells[row * cols..(row + 1) * cols];
        let mut col = 0;
        while col < cols {
            let bg = cells[col].bg;
            let start = col;
            while col < cols && cells[col].bg == bg {
                col += 1;
            }
            if bg != frame.bg {
                bg_mesh.add_colored_rect(
                    Rect::from_min_max(
                        pos2(x_of(start), y_of(row)),
                        pos2(x_of(col), y_of(row + 1)),
                    ),
                    bg,
                );
            }
        }
    }
    if let Some(c) = block_cursor {
        let w = if c.wide { 2 } else { 1 };
        bg_mesh.add_colored_rect(
            Rect::from_min_max(
                pos2(x_of(c.col), y_of(c.row)),
                pos2(x_of(c.col + w), y_of(c.row + 1)),
            ),
            frame.cursor_color,
        );
    }
    if !bg_mesh.is_empty() {
        painter.add(Shape::mesh(bg_mesh));
    }
    paint_images(frame, painter, p.graphics, m, ox_px, oy_px);

    // 2. Text rows (+ per-cell special glyphs), 3. decorations into the foreground mesh.
    let mut fg_mesh = Mesh::default();
    let line_px = (m.cell_h_px / 18.0).round().max(1.0);
    let underline_top = ((m.cell_h_px * 0.86).round()).min(m.cell_h_px - line_px);
    let strike_top = (m.cell_h_px * 0.52).round() - (line_px / 2.0).floor();
    let regular = FontId::new(m.font_size, m.regular.clone());
    let bold = FontId::new(m.font_size, m.bold.clone());

    for row in 0..rows {
        let cells = &frame.cells[row * cols..(row + 1) * cols];
        // Columns past the last visible glyph need no text.
        let last = cells
            .iter()
            .rposition(|c| needs_glyph(c.c) && !is_spacer(c.flags));
        scratch.specials.clear();
        let y_px = oy_px + row as f32 * m.cell_h_px;

        // Decorations first pass (independent of glyph class).
        for (col, cell) in cells.iter().enumerate() {
            if is_spacer(cell.flags) {
                continue;
            }
            let x_px = ox_px + col as f32 * m.cell_w_px;
            let span = if cell.flags.contains(Flags::WIDE_CHAR) {
                2.0
            } else {
                1.0
            };
            let mut cp = CellPx {
                mesh: &mut fg_mesh,
                x0: x_px,
                y0: y_px,
                w: m.cell_w_px * span,
                h: m.cell_h_px,
                ppp: m.ppp,
                color: cell.underline,
            };
            let hovered = (p.hover_link != 0 && cell.link == p.hover_link)
                || p.hover_url.is_some_and(|u| u.contains(row, col));
            draw_underline(&mut cp, cell.flags, hovered, underline_top, line_px);
            if cell.flags.contains(Flags::STRIKEOUT) {
                cp.color = cell.fg;
                cp.rect(0.0, strike_top, cp.w, strike_top + line_px);
            }
            if is_procedural(cell.c) && !cell.flags.contains(Flags::HIDDEN) {
                let mut fg = cell.fg;
                if block_cursor.is_some_and(|c| c.row == row && c.col == col) {
                    fg = cell.bg;
                }
                cp.color = fg;
                cp.w = m.cell_w_px;
                draw_procedural(&mut cp, cell.c);
            }
        }

        let Some(last) = last else { continue };
        let mut job = LayoutJob::default();
        job.text.reserve(last + 1);
        let mut cur: Option<RunStyle> = None;
        let mut section_start = 0usize;
        for (col, cell) in cells.iter().enumerate().take(last + 1) {
            let ch = if is_spacer(cell.flags) || !needs_glyph(cell.c) || is_procedural(cell.c) {
                ' '
            } else if cell.zw_len == 0
                && !cell.flags.contains(Flags::WIDE_CHAR)
                && glyphs.fits_row(ctx, m, cell.c)
            {
                cell.c
            } else {
                scratch.specials.push(col);
                ' '
            };
            if ch != ' ' {
                let mut fg = cell.fg;
                if block_cursor.is_some_and(|c| c.row == row && c.col == col) {
                    fg = cell.bg;
                }
                let style = RunStyle {
                    fg,
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                };
                if cur != Some(style) {
                    if let Some(prev) = cur {
                        push_section(&mut job, section_start, prev, &regular, &bold, m);
                    }
                    section_start = job.text.len();
                    cur = Some(style);
                }
            }
            job.text.push(ch);
        }
        if let Some(style) = cur {
            push_section(&mut job, section_start, style, &regular, &bold, m);
            // Leading blanks before the first styled char belong to the first section.
            if let Some(first) = job.sections.first_mut() {
                first.byte_range.start = ByteIndex(0);
            }
            let galley = painter.layout_job(job);
            painter.galley(pos2(x_of(0), y_of(row)), galley, Color32::WHITE);
        }

        // Per-cell glyphs: fallback faces, wide chars, combining clusters.
        for &col in &scratch.specials {
            let cell = &cells[col];
            let zs = cell.zw_start as usize;
            let zw = &frame.zerowidth[zs..zs + usize::from(cell.zw_len)];
            let mut fg = cell.fg;
            if block_cursor.is_some_and(|c| c.row == row && c.col == col) {
                fg = cell.bg;
            }
            let is_bold = cell.flags.contains(Flags::BOLD);
            let font = if is_bold { &bold } else { &regular };
            let style = (is_bold, cell.flags.contains(Flags::ITALIC));
            let galley = glyphs.special(painter, font, m, cell.c, zw, style);
            let span = if cell.flags.contains(Flags::WIDE_CHAR) {
                2.0
            } else {
                1.0
            };
            let slack_px = (span * m.cell_w_px - galley.size().x * m.ppp) / 2.0;
            let x = (ox_px + col as f32 * m.cell_w_px + slack_px.round()) / m.ppp;
            painter.galley(pos2(x, y_of(row)), galley, fg);
        }
    }

    // Non-block cursor shapes.
    if let Some(c) = cursor {
        let span = if c.wide { 2.0 } else { 1.0 };
        let mut cp = CellPx {
            mesh: &mut fg_mesh,
            x0: ox_px + c.col as f32 * m.cell_w_px,
            y0: oy_px + c.row as f32 * m.cell_h_px,
            w: m.cell_w_px * span,
            h: m.cell_h_px,
            ppp: m.ppp,
            color: frame.cursor_color,
        };
        let thick = (m.cell_w_px / 8.0).round().max(1.0);
        let shape = if p.focused {
            c.shape
        } else {
            CursorShape::HollowBlock
        };
        match shape {
            CursorShape::Underline => cp.rect(0.0, cp.h - thick, cp.w, cp.h),
            CursorShape::Beam => cp.rect(0.0, 0.0, thick, cp.h),
            CursorShape::HollowBlock => {
                let t = line_px;
                cp.rect(0.0, 0.0, cp.w, t);
                cp.rect(0.0, cp.h - t, cp.w, cp.h);
                cp.rect(0.0, t, t, cp.h - t);
                cp.rect(cp.w - t, t, cp.w, cp.h - t);
            }
            CursorShape::Block | CursorShape::Hidden => {}
        }
    }
    if !fg_mesh.is_empty() {
        painter.add(Shape::mesh(fg_mesh));
    }

    // IME pre-edit text at the cursor.
    if !p.preedit.is_empty()
        && let Some(c) = frame.cursor
    {
        let format = TextFormat {
            font_id: regular.clone(),
            color: frame.bg,
            background: frame.cursor_color,
            expand_bg: 0.0,
            extra_letter_spacing: m.spacing,
            line_height: Some(m.cell_h),
            underline: Stroke::new(1.0, frame.bg),
            ..TextFormat::default()
        };
        let galley = painter.layout_job(LayoutJob::single_section(p.preedit.to_owned(), format));
        painter.galley(pos2(x_of(c.col), y_of(c.row)), galley, Color32::WHITE);
    }

    // Exit banner over the bottom row.
    if let (Some(text), true) = (p.banner, rows > 0) {
        let format = TextFormat {
            font_id: bold.clone(),
            color: frame.bg,
            extra_letter_spacing: m.spacing,
            line_height: Some(m.cell_h),
            ..TextFormat::default()
        };
        let galley = painter.layout_job(LayoutJob::single_section(text.to_owned(), format));
        let y = y_of(rows - 1);
        let w = galley.size().x + 2.0 * m.cell_w;
        painter.rect_filled(
            Rect::from_min_size(pos2(x_of(0), y), vec2(w, m.cell_h)),
            0.0,
            frame.cursor_color,
        );
        painter.galley(pos2(x_of(0) + m.cell_w, y), galley, Color32::WHITE);
    }
}

/// Draws the image part of every placeholder cell: one textured mesh per image.
fn paint_images(
    frame: &Frame,
    painter: &Painter,
    graphics: &Graphics,
    m: &Metrics,
    ox_px: f32,
    oy_px: f32,
) {
    if frame.placeholders.is_empty() {
        return;
    }
    let ctx = painter.ctx();
    let cell = [m.cell_w_px, m.cell_h_px];
    let mut meshes: Vec<Mesh> = Vec::new();
    let mut store = graphics.paint_lock();
    // Placeholder runs share one image: look it up once per run.
    let mut last = None;
    for ph in &frame.placeholders {
        let target = match last {
            Some((id, pid, t)) if (id, pid) == (ph.id, ph.pid) => t,
            _ => {
                let t = store.target(ctx, ph.id, ph.pid);
                last = Some((ph.id, ph.pid, t));
                t
            }
        };
        let Some((texture, size, box_cells)) = target else {
            continue;
        };
        let box_cells = graphics::resolve_box(box_cells, size, cell);
        let Some((local, uv)) = graphics::cell_quad(box_cells, size, cell, ph.img_row, ph.img_col)
        else {
            continue;
        };
        let x0 = ox_px + ph.col as f32 * m.cell_w_px;
        let y0 = oy_px + ph.row as f32 * m.cell_h_px;
        let rect = Rect::from_min_max(
            pos2((x0 + local.min.x) / m.ppp, (y0 + local.min.y) / m.ppp),
            pos2((x0 + local.max.x) / m.ppp, (y0 + local.max.y) / m.ppp),
        );
        let i = match meshes.iter().position(|mesh| mesh.texture_id == texture) {
            Some(i) => i,
            None => {
                meshes.push(Mesh::with_texture(texture));
                meshes.len() - 1
            }
        };
        meshes[i].add_rect_with_uv(rect, uv, Color32::WHITE);
    }
    drop(store);
    for mesh in meshes {
        painter.add(Shape::mesh(mesh));
    }
}

fn push_section(
    job: &mut LayoutJob,
    start: usize,
    style: RunStyle,
    regular: &FontId,
    bold: &FontId,
    m: &Metrics,
) {
    job.sections.push(LayoutSection {
        leading_space: 0.0,
        byte_range: ByteIndex(start)..ByteIndex(job.text.len()),
        format: TextFormat {
            font_id: if style.bold {
                bold.clone()
            } else {
                regular.clone()
            },
            extra_letter_spacing: m.spacing,
            line_height: Some(m.cell_h),
            color: style.fg,
            expand_bg: 0.0,
            italics: style.italic,
            ..TextFormat::default()
        },
    });
}

fn is_spacer(flags: Flags) -> bool {
    flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

/// Blank or control: nothing to draw (controls would render as tofu).
fn needs_glyph(c: char) -> bool {
    c != ' ' && c != '\u{a0}' && !c.is_control()
}

pub(crate) fn is_procedural(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x259F | 0x2801..=0x28FF | 0xE0B0..=0xE0B3)
}

fn draw_underline(cp: &mut CellPx<'_>, flags: Flags, hovered_link: bool, top: f32, t: f32) {
    let w = cp.w;
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        let top2 = (top - 2.0 * t).max(0.0);
        cp.rect(0.0, top2, w, top2 + t);
        cp.rect(0.0, top, w, top + t);
    } else if flags.contains(Flags::UNDERCURL) {
        // One sine-like period per cell, amplitude t*1.5 around the underline.
        let amp = (t * 1.5).max(1.5);
        let mid = top + t / 2.0;
        let pts = [
            (0.0, mid),
            (w / 4.0, mid - amp),
            (w * 3.0 / 4.0, mid + amp),
            (w, mid),
        ];
        for pair in pts.windows(2) {
            cp.line(pair[0], pair[1], t);
        }
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        let step = 2.0 * t;
        // Phase from the absolute x so dots continue across cells.
        let mut x = (step - (cp.x0 % step)) % step;
        while x < w {
            cp.rect(x, top, (x + t).min(w), top + t);
            x += step;
        }
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        let dash = (w * 0.6).round().max(1.0);
        let start = ((w - dash) / 2.0).floor();
        cp.rect(start, top, start + dash, top + t);
    } else if flags.contains(Flags::UNDERLINE) || hovered_link {
        cp.rect(0.0, top, w, top + t);
    }
}

/// One cell in physical-pixel coordinates; emits mesh geometry in points.
pub(crate) struct CellPx<'a> {
    pub mesh: &'a mut Mesh,
    pub x0: f32,
    pub y0: f32,
    pub w: f32,
    pub h: f32,
    pub ppp: f32,
    pub color: Color32,
}

impl CellPx<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        pos2((self.x0 + x) / self.ppp, (self.y0 + y) / self.ppp)
    }

    fn rect(&mut self, l: f32, t: f32, r: f32, b: f32) {
        if r > l && b > t {
            let rect = Rect::from_min_max(self.pt(l, t), self.pt(r, b));
            self.mesh.add_colored_rect(rect, self.color);
        }
    }

    fn tri(&mut self, a: (f32, f32), b: (f32, f32), c: (f32, f32)) {
        let i = self.mesh.vertices.len() as u32;
        for v in [a, b, c] {
            let p = self.pt(v.0, v.1);
            self.mesh.colored_vertex(p, self.color);
        }
        self.mesh.add_triangle(i, i + 1, i + 2);
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32), width: f32) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len <= f32::EPSILON {
            return;
        }
        let (nx, ny) = (-dy / len * width / 2.0, dx / len * width / 2.0);
        let i = self.mesh.vertices.len() as u32;
        for v in [
            (a.0 + nx, a.1 + ny),
            (a.0 - nx, a.1 - ny),
            (b.0 - nx, b.1 - ny),
            (b.0 + nx, b.1 + ny),
        ] {
            let p = self.pt(v.0, v.1);
            self.mesh.colored_vertex(p, self.color);
        }
        self.mesh.add_triangle(i, i + 1, i + 2);
        self.mesh.add_triangle(i, i + 2, i + 3);
    }
}

/// Line weights (up, right, down, left) for U+2500..=U+257F: 0 none, 1 light, 2 heavy,
/// 3 double. Arcs/diagonals (U+256D..=U+2573) are handled separately.
const BOX: [[u8; 4]; 128] = [
    [0, 1, 0, 1],
    [0, 2, 0, 2],
    [1, 0, 1, 0],
    [2, 0, 2, 0], // 2500 ─ ━ │ ┃
    [0, 1, 0, 1],
    [0, 2, 0, 2],
    [1, 0, 1, 0],
    [2, 0, 2, 0], // 2504 ┄ ┅ ┆ ┇
    [0, 1, 0, 1],
    [0, 2, 0, 2],
    [1, 0, 1, 0],
    [2, 0, 2, 0], // 2508 ┈ ┉ ┊ ┋
    [0, 1, 1, 0],
    [0, 2, 1, 0],
    [0, 1, 2, 0],
    [0, 2, 2, 0], // 250C ┌ ┍ ┎ ┏
    [0, 0, 1, 1],
    [0, 0, 1, 2],
    [0, 0, 2, 1],
    [0, 0, 2, 2], // 2510 ┐ ┑ ┒ ┓
    [1, 1, 0, 0],
    [1, 2, 0, 0],
    [2, 1, 0, 0],
    [2, 2, 0, 0], // 2514 └ ┕ ┖ ┗
    [1, 0, 0, 1],
    [1, 0, 0, 2],
    [2, 0, 0, 1],
    [2, 0, 0, 2], // 2518 ┘ ┙ ┚ ┛
    [1, 1, 1, 0],
    [1, 2, 1, 0],
    [2, 1, 1, 0],
    [1, 1, 2, 0], // 251C ├ ┝ ┞ ┟
    [2, 1, 2, 0],
    [2, 2, 1, 0],
    [1, 2, 2, 0],
    [2, 2, 2, 0], // 2520 ┠ ┡ ┢ ┣
    [1, 0, 1, 1],
    [1, 0, 1, 2],
    [2, 0, 1, 1],
    [1, 0, 2, 1], // 2524 ┤ ┥ ┦ ┧
    [2, 0, 2, 1],
    [2, 0, 1, 2],
    [1, 0, 2, 2],
    [2, 0, 2, 2], // 2528 ┨ ┩ ┪ ┫
    [0, 1, 1, 1],
    [0, 1, 1, 2],
    [0, 2, 1, 1],
    [0, 2, 1, 2], // 252C ┬ ┭ ┮ ┯
    [0, 1, 2, 1],
    [0, 1, 2, 2],
    [0, 2, 2, 1],
    [0, 2, 2, 2], // 2530 ┰ ┱ ┲ ┳
    [1, 1, 0, 1],
    [1, 1, 0, 2],
    [1, 2, 0, 1],
    [1, 2, 0, 2], // 2534 ┴ ┵ ┶ ┷
    [2, 1, 0, 1],
    [2, 1, 0, 2],
    [2, 2, 0, 1],
    [2, 2, 0, 2], // 2538 ┸ ┹ ┺ ┻
    [1, 1, 1, 1],
    [1, 1, 1, 2],
    [1, 2, 1, 1],
    [1, 2, 1, 2], // 253C ┼ ┽ ┾ ┿
    [2, 1, 1, 1],
    [1, 1, 2, 1],
    [2, 1, 2, 1],
    [2, 1, 1, 2], // 2540 ╀ ╁ ╂ ╃
    [2, 2, 1, 1],
    [1, 1, 2, 2],
    [1, 2, 2, 1],
    [2, 2, 1, 2], // 2544 ╄ ╅ ╆ ╇
    [1, 2, 2, 2],
    [2, 1, 2, 2],
    [2, 2, 2, 1],
    [2, 2, 2, 2], // 2548 ╈ ╉ ╊ ╋
    [0, 1, 0, 1],
    [0, 2, 0, 2],
    [1, 0, 1, 0],
    [2, 0, 2, 0], // 254C ╌ ╍ ╎ ╏
    [0, 3, 0, 3],
    [3, 0, 3, 0],
    [0, 3, 1, 0],
    [0, 1, 3, 0], // 2550 ═ ║ ╒ ╓
    [0, 3, 3, 0],
    [0, 0, 1, 3],
    [0, 0, 3, 1],
    [0, 0, 3, 3], // 2554 ╔ ╕ ╖ ╗
    [1, 3, 0, 0],
    [3, 1, 0, 0],
    [3, 3, 0, 0],
    [1, 0, 0, 3], // 2558 ╘ ╙ ╚ ╛
    [3, 0, 0, 1],
    [3, 0, 0, 3],
    [1, 3, 1, 0],
    [3, 1, 3, 0], // 255C ╜ ╝ ╞ ╟
    [3, 3, 3, 0],
    [1, 0, 1, 3],
    [3, 0, 3, 1],
    [3, 0, 3, 3], // 2560 ╠ ╡ ╢ ╣
    [0, 3, 1, 3],
    [0, 1, 3, 1],
    [0, 3, 3, 3],
    [1, 3, 0, 3], // 2564 ╤ ╥ ╦ ╧
    [3, 1, 0, 1],
    [3, 3, 0, 3],
    [1, 3, 1, 3],
    [3, 1, 3, 1], // 2568 ╨ ╩ ╪ ╫
    [3, 3, 3, 3],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0], // 256C ╬ ╭ ╮ ╯
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0], // 2570 ╰ ╱ ╲ ╳
    [0, 0, 0, 1],
    [1, 0, 0, 0],
    [0, 1, 0, 0],
    [0, 0, 1, 0], // 2574 ╴ ╵ ╶ ╷
    [0, 0, 0, 2],
    [2, 0, 0, 0],
    [0, 2, 0, 0],
    [0, 0, 2, 0], // 2578 ╸ ╹ ╺ ╻
    [0, 2, 0, 1],
    [1, 0, 2, 0],
    [0, 1, 0, 2],
    [2, 0, 1, 0], // 257C ╼ ╽ ╾ ╿
];

/// Draws U+2500–257F, U+2580–259F, braille and powerline glyphs into `cp`.
pub(crate) fn draw_procedural(cp: &mut CellPx<'_>, c: char) {
    let u = c as u32;
    let (w, h) = (cp.w, cp.h);
    let light = (w / 8.0).round().max(1.0);
    match u {
        0x256D..=0x2570 => {
            let (dx, dy) = match u {
                0x256D => (1.0, 1.0),
                0x256E => (-1.0, 1.0),
                0x256F => (-1.0, -1.0),
                _ => (1.0, -1.0),
            };
            draw_arc(cp, dx, dy, light);
        }
        0x2571 => cp.line((w, 0.0), (0.0, h), light),
        0x2572 => cp.line((0.0, 0.0), (w, h), light),
        0x2573 => {
            cp.line((w, 0.0), (0.0, h), light);
            cp.line((0.0, 0.0), (w, h), light);
        }
        0x2550..=0x256C => draw_double(cp, BOX[(u - 0x2500) as usize], light),
        0x2500..=0x257F => {
            let dashes = match u {
                0x2504..=0x2507 => 3,
                0x2508..=0x250B => 4,
                0x254C..=0x254F => 2,
                _ => 0,
            };
            draw_lines(cp, BOX[(u - 0x2500) as usize], light, dashes);
        }
        0x2580..=0x259F => draw_block(cp, u),
        0x2800..=0x28FF => draw_braille(cp, u - 0x2800),
        0xE0B0 => cp.tri((0.0, 0.0), (w, h / 2.0), (0.0, h)),
        0xE0B2 => cp.tri((w, 0.0), (0.0, h / 2.0), (w, h)),
        0xE0B1 => {
            cp.line((0.0, 0.0), (w, h / 2.0), light);
            cp.line((w, h / 2.0), (0.0, h), light);
        }
        0xE0B3 => {
            cp.line((w, 0.0), (0.0, h / 2.0), light);
            cp.line((0.0, h / 2.0), (w, h), light);
        }
        _ => {}
    }
}

fn draw_lines(cp: &mut CellPx<'_>, [up, right, down, left]: [u8; 4], light: f32, dashes: u32) {
    let (w, h) = (cp.w, cp.h);
    let heavy = light * 2.0;
    let t = |v: u8| match v {
        1 => light,
        2 => heavy,
        _ => 0.0,
    };
    let (cx, cy) = ((w / 2.0).floor(), (h / 2.0).floor());
    let tv = t(up).max(t(down));
    let th = t(left).max(t(right));
    if dashes > 0 {
        let n = dashes as f32;
        if left > 0 {
            let tt = t(left);
            let y = cy - (tt / 2.0).floor();
            let seg = w / n;
            for i in 0..dashes {
                let x = i as f32 * seg;
                cp.rect(
                    x.round(),
                    y,
                    (x + seg * 0.6).round().max(x.round() + 1.0),
                    y + tt,
                );
            }
        } else {
            let tt = t(up);
            let x = cx - (tt / 2.0).floor();
            let seg = h / n;
            for i in 0..dashes {
                let y = i as f32 * seg;
                cp.rect(
                    x,
                    y.round(),
                    x + tt,
                    (y + seg * 0.6).round().max(y.round() + 1.0),
                );
            }
        }
        return;
    }
    if left > 0 {
        let tt = t(left);
        let y = cy - (tt / 2.0).floor();
        let end = (cx - (tt / 2.0).floor() + tt).max(cx - (tv / 2.0).floor() + tv);
        cp.rect(0.0, y, end, y + tt);
    }
    if right > 0 {
        let tt = t(right);
        let y = cy - (tt / 2.0).floor();
        let start = (cx - (tt / 2.0).floor()).min(cx - (tv / 2.0).floor());
        cp.rect(start, y, w, y + tt);
    }
    if up > 0 {
        let tt = t(up);
        let x = cx - (tt / 2.0).floor();
        let end = (cy - (tt / 2.0).floor() + tt).max(cy - (th / 2.0).floor() + th);
        cp.rect(x, 0.0, x + tt, end);
    }
    if down > 0 {
        let tt = t(down);
        let x = cx - (tt / 2.0).floor();
        let start = (cy - (tt / 2.0).floor()).min(cy - (th / 2.0).floor());
        cp.rect(x, start, x + tt, h);
    }
}

/// Double/single mixes (U+2550–256C): double arms are two `lt` lines separated by `lt`.
fn draw_double(cp: &mut CellPx<'_>, [up, right, down, left]: [u8; 4], lt: f32) {
    let (w, h) = (cp.w, cp.h);
    let (cx, cy) = ((w / 2.0).floor(), (h / 2.0).floor());
    // Pair origins: horizontal pair occupies rows [p, p+3lt), vertical pair columns [q, q+3lt).
    let p = cy - (1.5 * lt).floor();
    let q = cx - (1.5 * lt).floor();
    let single_y = cy - (lt / 2.0).floor();
    let single_x = cx - (lt / 2.0).floor();
    let vert_double = up == 3 || down == 3;
    let horz_double = left == 3 || right == 3;
    let vert_single = up == 1 || down == 1;
    let horz_single = left == 1 || right == 1;

    // Horizontal arms.
    for (arm, is_right) in [(left, false), (right, true)] {
        match arm {
            3 => {
                // (line y, the vertical arm on this line's side, the opposite one)
                for (y, side, other) in [(p, up, down), (p + 2.0 * lt, down, up)] {
                    let junction = if side == 3 {
                        if is_right { q + 2.0 * lt } else { q + lt }
                    } else if other == 3 {
                        if is_right { q } else { q + 3.0 * lt }
                    } else if vert_single {
                        if is_right { single_x } else { single_x + lt }
                    } else {
                        cx
                    };
                    if is_right {
                        cp.rect(junction, y, w, y + lt);
                    } else {
                        cp.rect(0.0, y, junction, y + lt);
                    }
                }
            }
            1 => {
                let (a, b) = if vert_double {
                    (q, q + 3.0 * lt)
                } else {
                    (single_x, single_x + lt)
                };
                if is_right {
                    cp.rect(a, single_y, w, single_y + lt);
                } else {
                    cp.rect(0.0, single_y, b, single_y + lt);
                }
            }
            _ => {}
        }
    }
    // Vertical arms.
    for (arm, is_down) in [(up, false), (down, true)] {
        match arm {
            3 => {
                for (x, side, other) in [(q, left, right), (q + 2.0 * lt, right, left)] {
                    let junction = if side == 3 {
                        if is_down { p + 2.0 * lt } else { p + lt }
                    } else if other == 3 {
                        if is_down { p } else { p + 3.0 * lt }
                    } else if horz_single {
                        if is_down { single_y } else { single_y + lt }
                    } else {
                        cy
                    };
                    if is_down {
                        cp.rect(x, junction, x + lt, h);
                    } else {
                        cp.rect(x, 0.0, x + lt, junction);
                    }
                }
            }
            1 => {
                let (a, b) = if horz_double {
                    (p, p + 3.0 * lt)
                } else {
                    (single_y, single_y + lt)
                };
                if is_down {
                    cp.rect(single_x, a, single_x + lt, h);
                } else {
                    cp.rect(single_x, 0.0, single_x + lt, b);
                }
            }
            _ => {}
        }
    }
}

/// Rounded corner opening towards (dx, dy) (±1): straight stubs to the edges + quarter circle.
fn draw_arc(cp: &mut CellPx<'_>, dx: f32, dy: f32, lt: f32) {
    let (w, h) = (cp.w, cp.h);
    let ccx = (w / 2.0).floor() - (lt / 2.0).floor() + lt / 2.0;
    let ccy = (h / 2.0).floor() - (lt / 2.0).floor() + lt / 2.0;
    let r = (w / 2.0).min(h / 2.0).floor().max(1.0);
    let (center_x, center_y) = (ccx + dx * r, ccy + dy * r);
    const STEPS: usize = 8;
    let mut prev = (ccx, ccy + dy * r);
    for i in 1..=STEPS {
        let a = i as f32 / STEPS as f32 * std::f32::consts::FRAC_PI_2;
        let pt = (center_x - dx * r * a.cos(), center_y - dy * r * a.sin());
        cp.line(prev, pt, lt);
        prev = pt;
    }
    let half = lt / 2.0;
    // Vertical stub from the arc's start to the top/bottom edge.
    let y_arc = ccy + dy * r;
    if dy > 0.0 {
        cp.rect(ccx - half, y_arc, ccx + half, h);
    } else {
        cp.rect(ccx - half, 0.0, ccx + half, y_arc);
    }
    let x_arc = ccx + dx * r;
    if dx > 0.0 {
        cp.rect(x_arc, ccy - half, w, ccy + half);
    } else {
        cp.rect(0.0, ccy - half, x_arc, ccy + half);
    }
}

fn draw_block(cp: &mut CellPx<'_>, u: u32) {
    let (w, h) = (cp.w, cp.h);
    let eighth_h = |n: f32| (h * n / 8.0).round();
    let eighth_w = |n: f32| (w * n / 8.0).round();
    let (hx, hy) = ((w / 2.0).round(), (h / 2.0).round());
    match u {
        0x2580 => cp.rect(0.0, 0.0, w, hy),
        0x2581..=0x2588 => {
            let n = (u - 0x2580) as f32;
            cp.rect(0.0, h - eighth_h(n), w, h);
        }
        0x2589..=0x258F => {
            let n = (0x2590 - u) as f32;
            cp.rect(0.0, 0.0, eighth_w(n), h);
        }
        0x2590 => cp.rect(hx, 0.0, w, h),
        0x2591..=0x2593 => {
            let alpha = [0.25, 0.5, 0.75][(u - 0x2591) as usize];
            let base = cp.color;
            cp.color = base.gamma_multiply(alpha);
            cp.rect(0.0, 0.0, w, h);
            cp.color = base;
        }
        0x2594 => cp.rect(0.0, 0.0, w, eighth_h(1.0)),
        0x2595 => cp.rect(w - eighth_w(1.0), 0.0, w, h),
        _ => {
            // Quadrants: bit 0 UL, 1 UR, 2 LL, 3 LR.
            let quads: u8 = match u {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                0x259F => 0b1110,
                _ => 0,
            };
            if quads & 1 != 0 {
                cp.rect(0.0, 0.0, hx, hy);
            }
            if quads & 2 != 0 {
                cp.rect(hx, 0.0, w, hy);
            }
            if quads & 4 != 0 {
                cp.rect(0.0, hy, hx, h);
            }
            if quads & 8 != 0 {
                cp.rect(hx, hy, w, h);
            }
        }
    }
}

fn draw_braille(cp: &mut CellPx<'_>, bits: u32) {
    let (w, h) = (cp.w, cp.h);
    let d = (w / 5.0).round().max(1.0);
    // (bit, column, row) per Unicode braille dot numbering.
    const DOTS: [(u32, f32, f32); 8] = [
        (0, 0.0, 0.0),
        (1, 0.0, 1.0),
        (2, 0.0, 2.0),
        (3, 1.0, 0.0),
        (4, 1.0, 1.0),
        (5, 1.0, 2.0),
        (6, 0.0, 3.0),
        (7, 1.0, 3.0),
    ];
    for (bit, col, row) in DOTS {
        if bits & (1 << bit) != 0 {
            let xc = w * (1.0 + 2.0 * col) / 4.0;
            let yc = h * (1.0 + 2.0 * row) / 8.0;
            let x = (xc - d / 2.0).round();
            let y = (yc - d / 2.0).round();
            cp.rect(x, y, x + d, y + d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::ClippedShape;

    /// `e`+U+0301, a wide `漢` (+ spacer), `x`.
    fn frame() -> Frame {
        let mut f = Frame {
            cols: 4,
            rows: 1,
            cells: vec![FCell::default(); 4],
            zerowidth: vec!['\u{301}'],
            ..Frame::default()
        };
        f.cells[0].c = 'e';
        f.cells[0].zw_len = 1;
        f.cells[1].c = '漢';
        f.cells[1].flags = Flags::WIDE_CHAR;
        f.cells[2].flags = Flags::WIDE_CHAR_SPACER;
        f.cells[3].c = 'x';
        f
    }

    /// Paints one egui pass; returns the text galleys painted.
    fn pass(ctx: &egui::Context, f: &Frame, glyphs: &mut GlyphCache) -> Vec<Arc<Galley>> {
        let mut scratch = PaintScratch::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let m = Metrics::compute(ui.ctx(), 14.0);
            let params = PaintParams {
                origin: Pos2::ZERO,
                metrics: &m,
                cursor_visible: false,
                focused: false,
                hover_link: 0,
                hover_url: None,
                graphics: &Graphics::new(ui.ctx().clone()),
                preedit: "",
                banner: None,
            };
            paint(f, ui.painter(), glyphs, &mut scratch, &params);
        });
        let shapes = std::mem::take(&mut out.shapes);
        out.drop_without_applying_deltas();
        shapes
            .into_iter()
            .filter_map(|ClippedShape { shape, .. }| match shape {
                Shape::Text(t) => Some(t.galley),
                _ => None,
            })
            .collect()
    }

    fn find<'a>(gs: &'a [Arc<Galley>], text: &str) -> &'a Arc<Galley> {
        let texts: Vec<_> = gs.iter().map(|g| g.text().to_owned()).collect();
        gs.iter()
            .find(|g| g.text() == text)
            .unwrap_or_else(|| panic!("no galley {text:?} in {texts:?}"))
    }

    #[test]
    fn combining_cluster_painted_with_its_mark() {
        let ctx = egui::Context::default();
        let mut glyphs = GlyphCache::default();
        let gs = pass(&ctx, &frame(), &mut glyphs);
        // One per-cell galley carries base + mark, so the shaper can compose them.
        find(&gs, "e\u{301}");
        find(&gs, "漢");
        // The row job keeps the grid: blanks where per-cell glyphs go.
        find(&gs, "   x");
    }

    #[test]
    fn special_galleys_reused_until_egui_resets_its_cache() {
        let ctx = egui::Context::default();
        let mut glyphs = GlyphCache::default();
        let f = frame();
        let first = pass(&ctx, &f, &mut glyphs);
        let second = pass(&ctx, &f, &mut glyphs);
        for text in ["e\u{301}", "漢"] {
            assert!(
                Arc::ptr_eq(find(&first, text), find(&second, text)),
                "{text:?} was laid out again"
            );
        }
        // A text-option change makes egui drop its galleys (atlas coordinates change).
        ctx.all_styles_mut(|s| {
            s.visuals.text_options.font_hinting = !s.visuals.text_options.font_hinting;
        });
        let third = pass(&ctx, &f, &mut glyphs);
        for text in ["e\u{301}", "漢"] {
            assert!(
                !Arc::ptr_eq(find(&second, text), find(&third, text)),
                "{text:?} survived an egui galley-cache reset"
            );
        }
    }
}
