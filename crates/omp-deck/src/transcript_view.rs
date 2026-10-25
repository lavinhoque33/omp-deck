//! The popup's transcript tab: a chat-style rendering of a subagent's [`TranscriptItem`]s.
//!
//! Thousands of items stay cheap: only rows intersecting the viewport are laid out. Row heights
//! are measured as rows are drawn (estimated before that) and kept in a prefix-sum table, so the
//! scrollbar is stable and a repaint is only requested when a measurement moved the layout.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::icons::Icon;
use crate::icons::{
    ICON_ARROW_LEFT_RIGHT, ICON_BELL, ICON_BRAIN, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT,
    ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_CLIPBOARD_LIST, ICON_FILE_TEXT, ICON_FOLD_VERTICAL,
    ICON_MESSAGES_SQUARE, ICON_NETWORK, ICON_PENCIL, ICON_POWER, ICON_SEARCH, ICON_TERMINAL,
    ICON_USER, ICON_WRENCH,
};
use deck_core::agents::TranscriptItem;
use deck_core::agents::markup::{self, Segment};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CursorIcon, FontFamily, FontId, Frame, Margin, RichText, ScrollArea,
    Sense, Stroke, Ui, vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::agent_model::short_model;
use crate::theme::{self, Palette};
use crate::widgets::{Spin, paint_icon};
use deck_service::statusfmt::fmt_tokens;

const GAP: f32 = 10.0;
const OVERSCAN: f32 = 400.0;
const HEADER_H: f32 = 28.0;
const RESULT_MAX_H: f32 = 280.0;
/// Collapsed-header preview length (chars).
const PREVIEW: usize = 160;
/// Wrapper blocks nest at most this deep before their body is shown verbatim.
const MAX_TAG_DEPTH: usize = 3;

/// One drawn row: a lone item, or a tool call merged with its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Single(usize),
    Tool { call: usize, result: Option<usize> },
}

/// Groups items into rows: a `ToolResult` joins the `ToolCall` with the same id (a result without
/// a call stands alone); empty assistant/thinking blocks are dropped.
pub fn build_rows(items: &[TranscriptItem]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(items.len());
    let mut pending: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        match item {
            TranscriptItem::ToolCall { call_id, .. } => {
                pending.insert(call_id.as_str(), rows.len());
                rows.push(Row::Tool {
                    call: i,
                    result: None,
                });
            }
            TranscriptItem::ToolResult { call_id, .. } => match pending.remove(call_id.as_str()) {
                Some(r) => {
                    if let Some(Row::Tool { result, .. }) = rows.get_mut(r) {
                        *result = Some(i);
                    }
                }
                None => rows.push(Row::Single(i)),
            },
            TranscriptItem::Assistant { text, .. } | TranscriptItem::Thinking { text }
                if text.trim().is_empty() => {}
            _ => rows.push(Row::Single(i)),
        }
    }
    rows
}

/// Rows start expanded when they are short or matter (steers, failed tools).
fn default_open(items: &[TranscriptItem], row: Row) -> bool {
    match row {
        Row::Tool { result, .. } => {
            matches!(
                result.and_then(|r| items.get(r)),
                Some(TranscriptItem::ToolResult { is_error: true, .. })
            )
        }
        Row::Single(i) => match &items[i] {
            TranscriptItem::User { from_agent, .. } => !from_agent,
            TranscriptItem::Notice { text, .. } => text.len() < 400,
            TranscriptItem::Irc { .. } => true,
            TranscriptItem::ToolResult { is_error, .. } => *is_error,
            _ => false,
        },
    }
}

fn estimate(items: &[TranscriptItem], row: Row, width: f32) -> f32 {
    let text_height = |t: &str| {
        let per_line = ((width - 56.0) / 7.2).max(20.0);
        let lines: f32 = t
            .lines()
            .map(|l| (l.chars().count() as f32 / per_line).ceil().max(1.0))
            .sum();
        (lines * 19.0).min(900.0)
    };
    match row {
        Row::Tool { .. } => HEADER_H + 14.0,
        Row::Single(i) => match &items[i] {
            TranscriptItem::Assistant { text, .. } => 52.0 + text_height(text),
            TranscriptItem::User { .. } | TranscriptItem::Thinking { .. } => HEADER_H + 22.0,
            TranscriptItem::Error { message } => 40.0 + text_height(message),
            TranscriptItem::Notice { text, .. } => 48.0 + text_height(text).min(160.0),
            TranscriptItem::Irc { text, .. } => HEADER_H + 24.0 + text_height(text),
            _ => 34.0,
        },
    }
}

/// Icon for a tool by name.
fn tool_icon(name: &str) -> Icon {
    match name {
        "read" => ICON_FILE_TEXT,
        "edit" | "write" | "ast_edit" => ICON_PENCIL,
        "bash" | "eval" | "ssh" => ICON_TERMINAL,
        "grep" | "find" | "web_search" | "ast_grep" | "search" => ICON_SEARCH,
        "task" | "wait" | "irc" => ICON_NETWORK,
        _ => ICON_WRENCH,
    }
}

/// A single-line, ellipsized layout of coloured parts, vertically centred on each other.
fn line_job(parts: &[(&str, FontId, Color32)], max_width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for (text, font, color) in parts {
        job.append(
            text,
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: *color,
                valign: Align::Center,
                ..TextFormat::default()
            },
        );
    }
    job.wrap.max_width = max_width.max(20.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

/// Right-hand state of a collapsible header.
enum Badge {
    None,
    Spinner(Color32),
    Icon(Icon, Color32),
}

struct Header<'a> {
    open: bool,
    icon: Icon,
    icon_color: Color32,
    title: &'a str,
    title_font: FontId,
    title_color: Color32,
    headline: &'a str,
    headline_color: Color32,
    tail: &'a str,
    badge: Badge,
}

/// A clickable header row; true when clicked (toggle).
fn header(ui: &mut Ui, h: &Header<'_>) -> bool {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 6, pal.raised.gamma_multiply(0.55));
    }
    let p = ui.painter().with_clip_rect(rect);
    let mid = rect.center().y;
    paint_icon(
        &p,
        egui::pos2(rect.left() + 10.0, mid),
        Align2::CENTER_CENTER,
        if h.open {
            ICON_CHEVRON_DOWN
        } else {
            ICON_CHEVRON_RIGHT
        },
        16.0,
        pal.weak,
    );
    paint_icon(
        &p,
        egui::pos2(rect.left() + 32.0, mid),
        Align2::CENTER_CENTER,
        h.icon,
        17.0,
        h.icon_color,
    );
    let mut right = rect.right() - 8.0;
    match &h.badge {
        Badge::None => {}
        Badge::Spinner(c) => {
            Spin::new(14.0, *c).paint_at(ui, egui::pos2(right - 7.0, mid));
            right -= 22.0;
        }
        Badge::Icon(icon, c) => {
            paint_icon(
                &p,
                egui::pos2(right - 8.0, mid),
                Align2::CENTER_CENTER,
                *icon,
                16.0,
                *c,
            );
            right -= 24.0;
        }
    }
    let x = rect.left() + 48.0;
    let mono = FontId::new(12.5, FontFamily::Monospace);
    let job = line_job(
        &[
            (h.title, h.title_font.clone(), h.title_color),
            ("  ", crate::theme::font(12.5), pal.weak),
            (h.headline, crate::theme::font(13.0), h.headline_color),
            (
                if h.tail.is_empty() { "" } else { "  " },
                mono.clone(),
                pal.weak,
            ),
            (h.tail, mono, pal.weak.gamma_multiply(0.85)),
        ],
        right - x,
    );
    let g = p.layout_job(job);
    p.galley(egui::pos2(x, mid - g.size().y / 2.0), g, pal.text);
    resp.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

/// Rounded surface whose left edge is a coloured bar; fills the available width.
fn bubble<R>(
    ui: &mut Ui,
    fill: Color32,
    bar: Color32,
    margin: Margin,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let pal = Palette::of_ui(ui);
    let radius = theme::rad(8, 10);
    let inner = Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, pal.stroke.gamma_multiply(0.8)))
        .corner_radius(radius)
        .inner_margin(margin)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    // The bubble's own rounded shape clipped to a 3 px strip, so the bar follows its corners.
    let r = inner.response.rect;
    let strip = egui::Rect::from_min_max(r.left_top(), egui::pos2(r.left() + 3.0, r.bottom()));
    ui.painter()
        .with_clip_rect(strip.intersect(ui.clip_rect()))
        .rect_filled(r, radius, bar);
    inner.inner
}

fn caption(ui: &mut Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(theme::section_caption(text, color)).selectable(false));
}

fn mono_block(ui: &mut Ui, text: &str, color: Color32, salt: (usize, &'static str), max_h: f32) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(theme::mix(pal.bg, pal.surface, 0.5))
        .corner_radius(theme::rad(6, 8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ScrollArea::vertical()
                .id_salt(salt)
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(text)
                                .font(FontId::new(12.0, FontFamily::Monospace))
                                .color(color),
                        )
                        .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                });
        });
}

/// Markdown in the reading style of [`theme::markdown_style`], its ATX headings drawn as
/// separate, weighted section titles (level 1–2 underlined by a hairline) so sections stand
/// apart from their bodies.
fn markdown(ui: &mut Ui, md: &mut CommonMarkCache, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.scope(|ui| {
        theme::markdown_style(ui.style_mut(), pal);
        for (n, (heading, body)) in markup::sections(text).into_iter().enumerate() {
            if let Some(h) = heading {
                section_heading(ui, h, n == 0);
            }
            let body = body.trim_matches('\n');
            if !body.trim().is_empty() {
                CommonMarkViewer::new().show(ui, md, body);
            }
        }
    });
}

/// A section title: weighted, a step above the body; Neon tints levels 1–2 with its cyan accent.
fn section_heading(ui: &mut Ui, h: markup::Heading<'_>, first: bool) {
    let pal = Palette::of_ui(ui);
    let body = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .map_or(13.0, |f| f.size);
    let major = if theme::flat() {
        pal.text_strong
    } else {
        pal.accent_b
    };
    let (size, color) = match h.level {
        1 => (body + 1.0, major),
        2 => (body, major),
        _ => (body - 1.0, pal.text_strong),
    };
    ui.add_space(if first { 2.0 } else { 12.0 });
    ui.add(
        egui::Label::new(
            theme::strong(markup::preview(h.text, 200))
                .size(size)
                .color(color),
        )
        .wrap_mode(egui::TextWrapMode::Wrap),
    );
    if h.level <= 2 {
        ui.add_space(3.0);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().hline(
            r.x_range(),
            r.center().y,
            Stroke::new(1.0, theme::mix(pal.stroke, pal.weak, 0.3)),
        );
    }
    ui.add_space(5.0);
}

/// `text` as markdown with its wrapper blocks (`<system-notice>`, `<context>`, …) drawn as
/// captioned subsections instead of literal tags.
fn rich_text(ui: &mut Ui, md: &mut CommonMarkCache, text: &str, depth: usize) {
    let pal = Palette::of_ui(ui);
    for seg in markup::segments(text) {
        match seg {
            Segment::Text(t) => markdown(ui, md, t.trim_matches('\n')),
            Segment::Tagged(t) => {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    caption(ui, &t.name.replace(['-', '_'], " "), pal.weak);
                    let attrs = t
                        .attrs
                        .iter()
                        .map(|(k, v)| {
                            if v.is_empty() {
                                (*k).to_owned()
                            } else {
                                format!("{k} {v}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    if !attrs.is_empty() {
                        ui.add(
                            egui::Label::new(RichText::new(attrs).size(11.0).color(pal.weak))
                                .truncate()
                                .selectable(false),
                        );
                    }
                });
                let body = t.body.trim_matches('\n');
                if !body.trim().is_empty() {
                    ui.add_space(3.0);
                    Frame::NONE
                        .fill(theme::mix(pal.bg, pal.surface, 0.5))
                        .stroke(Stroke::new(1.0, pal.stroke.gamma_multiply(0.6)))
                        .corner_radius(theme::rad(6, 8))
                        .inner_margin(Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if t.name == "output" || depth >= MAX_TAG_DEPTH {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(body)
                                            .font(FontId::new(12.0, FontFamily::Monospace))
                                            .color(pal.text),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                                );
                            } else {
                                rich_text(ui, md, body, depth + 1);
                            }
                        });
                }
                ui.add_space(6.0);
            }
        }
    }
}

/// Live state of the transcript tab.
pub struct TranscriptView {
    revision: Option<u64>,
    items: Arc<Vec<TranscriptItem>>,
    rows: Vec<Row>,
    heights: Vec<f32>,
    offsets: Vec<f32>,
    width: f32,
    /// Rows whose open state differs from [`default_open`].
    flipped: HashSet<usize>,
    pub follow: bool,
    jump: bool,
    md: CommonMarkCache,
}

impl TranscriptView {
    pub fn new(follow: bool) -> TranscriptView {
        TranscriptView {
            revision: None,
            items: Arc::new(Vec::new()),
            rows: Vec::new(),
            heights: Vec::new(),
            offsets: Vec::new(),
            width: 0.0,
            flipped: HashSet::new(),
            follow,
            jump: follow,
            md: CommonMarkCache::default(),
        }
    }

    /// Scroll to the end and keep following.
    pub fn jump_to_end(&mut self) {
        self.follow = true;
        self.jump = true;
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Adopt the watcher's list when its revision moved.
    pub fn sync(&mut self, revision: u64, items: impl FnOnce() -> Arc<Vec<TranscriptItem>>) {
        if self.revision == Some(revision) {
            return;
        }
        self.revision = Some(revision);
        let items = items();
        // Appends keep the retained prefix; a shrink or an atomic rewrite (same or longer list
        // with different content) invalidates every per-row cache.
        let replaced = items.len() < self.items.len()
            || self.items.last().is_some_and(|last| {
                items.first() != self.items.first() || items.get(self.items.len() - 1) != Some(last)
            });
        self.items = items;
        self.rows = build_rows(&self.items);
        if replaced {
            self.heights.clear();
            self.flipped.clear();
        }
        let have = self.heights.len();
        for i in have..self.rows.len() {
            self.heights
                .push(estimate(&self.items, self.rows[i], self.width.max(400.0)));
        }
        self.heights.truncate(self.rows.len());
    }

    fn reestimate(&mut self) {
        for (h, row) in self.heights.iter_mut().zip(&self.rows) {
            *h = estimate(&self.items, *row, self.width);
        }
    }

    /// Draw the transcript; `running` animates pending tool calls.
    pub fn show(&mut self, ui: &mut Ui, running: bool) {
        let items = Arc::clone(&self.items);
        let mut moved = false;
        let out = ScrollArea::vertical()
            .id_salt("agent-transcript")
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow)
            .show_viewport(ui, |ui, viewport| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let w = ui.available_width();
                if (w - self.width).abs() > 1.0 {
                    self.width = w;
                    self.reestimate();
                }
                self.offsets.clear();
                self.offsets.push(0.0);
                let mut y = 0.0;
                for h in &self.heights {
                    y += h + GAP;
                    self.offsets.push(y);
                }
                let total = y;
                let n = self.rows.len();
                let lo = viewport.min.y - OVERSCAN;
                let hi = viewport.max.y + OVERSCAN;
                let first = self.offsets[1..].partition_point(|&o| o <= lo).min(n);
                let end = self.offsets[..n].partition_point(|&o| o < hi).max(first);
                ui.add_space(self.offsets[first]);
                for i in first..end {
                    let row = self.rows[i];
                    let h = ui
                        .scope(|ui| self.draw_row(ui, &items, i, row, running))
                        .response
                        .rect
                        .height();
                    if (h - self.heights[i]).abs() > 0.5 {
                        self.heights[i] = h;
                        moved = true;
                    }
                    ui.add_space(GAP);
                }
                ui.add_space((total - self.offsets[end]).max(0.0));
                let (marker, _) = ui.allocate_exact_size(vec2(1.0, 1.0), Sense::hover());
                if self.jump {
                    ui.scroll_to_rect(marker, Some(Align::BOTTOM));
                    self.jump = false;
                    moved = true;
                }
            });
        if moved {
            ui.ctx().request_repaint();
        }
        // The wheel (or a drag on the scrollbar strip) away from the end releases "follow";
        // reaching the end again re-engages it.
        let at_end =
            out.state.clamped_offset().y + out.inner_rect.height() >= out.content_size.y - 6.0;
        let user = ui.input(|i| {
            i.smooth_scroll_delta.y != 0.0
                || (i.pointer.primary_down()
                    && i.pointer.interact_pos().is_some_and(|p| {
                        out.inner_rect.right() - 14.0 <= p.x && out.inner_rect.contains(p)
                    }))
        });
        if user {
            self.follow = at_end;
        }
    }

    fn is_open(&self, items: &[TranscriptItem], i: usize, row: Row) -> bool {
        default_open(items, row) ^ self.flipped.contains(&i)
    }

    fn toggle(&mut self, i: usize) {
        if !self.flipped.remove(&i) {
            self.flipped.insert(i);
        }
    }

    fn draw_row(
        &mut self,
        ui: &mut Ui,
        items: &[TranscriptItem],
        i: usize,
        row: Row,
        running: bool,
    ) {
        let pal = Palette::of_ui(ui);
        let open = self.is_open(items, i, row);
        let margin = Margin::symmetric(12, 8);
        match row {
            Row::Tool { call, result } => {
                let TranscriptItem::ToolCall {
                    name,
                    intent,
                    summary,
                    args,
                    ..
                } = &items[call]
                else {
                    return;
                };
                let res = result.and_then(|r| match &items[r] {
                    TranscriptItem::ToolResult {
                        text,
                        is_error,
                        truncated,
                        ..
                    } => Some((text.as_str(), *is_error, *truncated)),
                    _ => None,
                });
                let failed = res.is_some_and(|r| r.1);
                let (fill, bar) = if failed {
                    (theme::mix(pal.surface, pal.danger, 0.08), pal.danger)
                } else {
                    (
                        pal.surface.gamma_multiply(0.9),
                        if theme::flat() {
                            pal.stroke
                        } else {
                            pal.accent.gamma_multiply(0.7)
                        },
                    )
                };
                let badge = match res {
                    None if running => Badge::Spinner(pal.accent_b),
                    None => Badge::Icon(ICON_BELL, pal.weak.gamma_multiply(0.5)),
                    Some((_, true, _)) => Badge::Icon(ICON_CIRCLE_ALERT, pal.danger),
                    Some(_) => Badge::Icon(ICON_CIRCLE_CHECK, pal.ok),
                };
                let (headline, tail) = match intent.as_deref().filter(|s| !s.is_empty()) {
                    Some(intent) => (intent, summary.as_str()),
                    None => (summary.as_str(), ""),
                };
                let clicked = bubble(ui, fill, bar, Margin::symmetric(8, 2), |ui| {
                    let clicked = header(
                        ui,
                        &Header {
                            open,
                            icon: tool_icon(name),
                            icon_color: if failed { pal.danger } else { pal.accent },
                            title: name,
                            title_font: FontId::new(12.5, FontFamily::Monospace),
                            title_color: pal.text_strong,
                            headline,
                            headline_color: pal.text,
                            tail,
                            badge,
                        },
                    );
                    if open {
                        ui.add_space(4.0);
                        if !args.trim().is_empty() {
                            caption(ui, "Arguments", pal.weak);
                            mono_block(ui, args, pal.weak, (i, "args"), 120.0);
                            ui.add_space(6.0);
                        }
                        match res {
                            Some((text, is_error, truncated)) => {
                                caption(
                                    ui,
                                    if is_error { "Error" } else { "Result" },
                                    if is_error { pal.danger } else { pal.weak },
                                );
                                let body = if text.trim().is_empty() {
                                    "(empty)"
                                } else {
                                    text
                                };
                                mono_block(
                                    ui,
                                    body,
                                    if is_error { pal.danger } else { pal.text },
                                    (i, "result"),
                                    RESULT_MAX_H,
                                );
                                if truncated {
                                    ui.label(
                                        RichText::new("… output truncated").small().color(pal.weak),
                                    );
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new(if running {
                                        "Waiting for the result…"
                                    } else {
                                        "No result recorded"
                                    })
                                    .small()
                                    .color(pal.weak),
                                );
                            }
                        }
                        ui.add_space(6.0);
                    }
                    clicked
                });
                if clicked {
                    self.toggle(i);
                }
            }
            Row::Single(idx) => self.draw_single(ui, &items[idx], i, open, margin),
        }
    }

    fn draw_single(
        &mut self,
        ui: &mut Ui,
        item: &TranscriptItem,
        i: usize,
        open: bool,
        margin: Margin,
    ) {
        let pal = Palette::of_ui(ui);
        match item {
            TranscriptItem::User {
                text, from_agent, ..
            } => {
                let (title, icon) = if *from_agent {
                    ("Assignment", ICON_CLIPBOARD_LIST)
                } else {
                    ("You", ICON_USER)
                };
                let preview = markup::preview(text, PREVIEW);
                let user_c = if theme::flat() {
                    pal.weak
                } else {
                    pal.accent_a
                };
                let clicked = bubble(
                    ui,
                    theme::accent_wash(pal.surface, pal.accent_a, 0.10),
                    user_c,
                    Margin::symmetric(8, 2),
                    |ui| {
                        let c = header(
                            ui,
                            &Header {
                                open,
                                icon,
                                icon_color: user_c,
                                title,
                                title_font: crate::theme::font(13.0),
                                title_color: pal.text_strong,
                                headline: if open { "" } else { &preview },
                                headline_color: pal.weak,
                                tail: "",
                                badge: Badge::None,
                            },
                        );
                        if open {
                            Frame::NONE
                                .inner_margin(Margin {
                                    left: 10,
                                    right: 8,
                                    top: 2,
                                    bottom: 8,
                                })
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    rich_text(ui, &mut self.md, markup::strip_preamble(text), 0);
                                });
                        }
                        c
                    },
                );
                if clicked {
                    self.toggle(i);
                }
            }
            TranscriptItem::Assistant { text, model, .. } => {
                let asst_c = if theme::flat() {
                    pal.stroke
                } else {
                    pal.accent_b
                };
                bubble(ui, pal.surface, asst_c, Margin::symmetric(14, 10), |ui| {
                    ui.horizontal(|ui| {
                        caption(
                            ui,
                            "Assistant",
                            if theme::flat() {
                                pal.weak
                            } else {
                                pal.accent_b
                            },
                        );
                        if let Some(m) = model {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(short_model(m)).size(11.0).color(pal.weak),
                                )
                                .selectable(false),
                            );
                        }
                    });
                    ui.add_space(2.0);
                    markdown(ui, &mut self.md, text);
                });
            }
            TranscriptItem::Thinking { text } => {
                let preview = markup::preview(text, PREVIEW);
                let clicked = header(
                    ui,
                    &Header {
                        open,
                        icon: ICON_BRAIN,
                        icon_color: pal.weak,
                        title: "Thinking",
                        title_font: crate::theme::font(12.5),
                        title_color: pal.weak,
                        headline: if open { "" } else { &preview },
                        headline_color: pal.weak.gamma_multiply(0.8),
                        tail: "",
                        badge: Badge::None,
                    },
                );
                if open {
                    Frame::NONE
                        .inner_margin(Margin {
                            left: 34,
                            right: 8,
                            top: 0,
                            bottom: 4,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text).italics().size(13.0).color(pal.weak),
                                )
                                .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        });
                }
                if clicked {
                    self.toggle(i);
                }
            }
            TranscriptItem::ToolResult {
                name,
                text,
                is_error,
                ..
            } => {
                bubble(
                    ui,
                    pal.surface.gamma_multiply(0.9),
                    pal.weak,
                    margin,
                    |ui| {
                        caption(
                            ui,
                            &format!("{name} result"),
                            if *is_error { pal.danger } else { pal.weak },
                        );
                        mono_block(
                            ui,
                            text,
                            if *is_error { pal.danger } else { pal.text },
                            (i, "orphan"),
                            RESULT_MAX_H,
                        );
                    },
                );
            }
            TranscriptItem::ToolCall { .. } => {}
            TranscriptItem::Notice { kind, text } => {
                let preview = markup::preview(text, PREVIEW);
                let clicked = bubble(
                    ui,
                    theme::mix(pal.surface, pal.warn, 0.06),
                    pal.warn.gamma_multiply(0.8),
                    Margin::symmetric(8, 2),
                    |ui| {
                        let c = header(
                            ui,
                            &Header {
                                open,
                                icon: ICON_BELL,
                                icon_color: pal.warn,
                                title: kind,
                                title_font: crate::theme::font(12.5),
                                title_color: pal.text_strong,
                                headline: if open { "" } else { &preview },
                                headline_color: pal.weak,
                                tail: "",
                                badge: Badge::None,
                            },
                        );
                        if open {
                            Frame::NONE
                                .inner_margin(Margin {
                                    left: 10,
                                    right: 8,
                                    top: 2,
                                    bottom: 8,
                                })
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    rich_text(ui, &mut self.md, text, 0);
                                });
                        }
                        c
                    },
                );
                if clicked {
                    self.toggle(i);
                }
            }
            TranscriptItem::Irc {
                from,
                agent,
                text,
                note,
            } => {
                let title = match (agent, from) {
                    (Some(a), Some(f)) if a != f => format!("IRC · from {a} ({f})"),
                    (Some(who), _) | (None, Some(who)) => format!("IRC · from {who}"),
                    (None, None) => "IRC".to_owned(),
                };
                let preview = if open {
                    note.clone().unwrap_or_default()
                } else {
                    markup::preview(text, PREVIEW)
                };
                let tint = if ui.visuals().dark_mode { 0.12 } else { 0.16 };
                let clicked = bubble(
                    ui,
                    theme::mix(pal.surface, pal.warn, tint),
                    pal.warn,
                    Margin::symmetric(8, 2),
                    |ui| {
                        let c = header(
                            ui,
                            &Header {
                                open,
                                icon: ICON_MESSAGES_SQUARE,
                                icon_color: pal.warn,
                                title: &title,
                                title_font: crate::theme::font(13.0),
                                title_color: pal.text_strong,
                                headline: &preview,
                                headline_color: pal.weak,
                                tail: "",
                                badge: Badge::None,
                            },
                        );
                        if open {
                            Frame::NONE
                                .inner_margin(Margin {
                                    left: 10,
                                    right: 8,
                                    top: 2,
                                    bottom: 8,
                                })
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    rich_text(ui, &mut self.md, text, 0);
                                });
                        }
                        c
                    },
                );
                if clicked {
                    self.toggle(i);
                }
            }
            TranscriptItem::Compaction {
                tokens_before,
                tokens_after,
                summary,
            } => {
                let title = match (tokens_before, tokens_after) {
                    (Some(b), Some(a)) => {
                        format!(
                            "Context compacted · {} → {} tok",
                            fmt_tokens(*b),
                            fmt_tokens(*a)
                        )
                    }
                    (Some(b), None) => format!("Context compacted · {} tok before", fmt_tokens(*b)),
                    _ => "Context compacted".to_owned(),
                };
                let clicked = header(
                    ui,
                    &Header {
                        open,
                        icon: ICON_FOLD_VERTICAL,
                        icon_color: pal.accent_b,
                        title: &title,
                        title_font: crate::theme::font(12.5),
                        title_color: pal.text,
                        headline: "",
                        headline_color: pal.weak,
                        tail: "",
                        badge: Badge::None,
                    },
                );
                if open {
                    Frame::NONE
                        .inner_margin(Margin {
                            left: 34,
                            right: 8,
                            top: 0,
                            bottom: 4,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            markdown(ui, &mut self.md, summary);
                        });
                }
                if clicked {
                    self.toggle(i);
                }
            }
            TranscriptItem::ModelChange { model } => {
                centered_note(
                    ui,
                    ICON_ARROW_LEFT_RIGHT,
                    &format!("Model → {model}"),
                    pal.weak,
                );
            }
            TranscriptItem::Error { message } => {
                bubble(
                    ui,
                    theme::mix(pal.surface, pal.danger, 0.10),
                    pal.danger,
                    Margin::symmetric(14, 10),
                    |ui| {
                        ui.horizontal_top(|ui| {
                            ui.label(
                                RichText::new(ICON_CIRCLE_ALERT.codepoint)
                                    .font(FontId::new(17.0, ICON_CIRCLE_ALERT.font_family()))
                                    .color(pal.danger),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(message).color(pal.danger))
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        });
                    },
                );
            }
            TranscriptItem::Exit { reason } => {
                centered_note(
                    ui,
                    ICON_POWER,
                    &format!("Session ended · {reason}"),
                    pal.weak,
                );
            }
        }
    }
}

fn centered_note(ui: &mut Ui, icon: Icon, text: &str, color: Color32) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let p = ui.painter();
    let g = p.layout_no_wrap(text.to_owned(), crate::theme::font(12.0), color);
    let w = g.size().x + 26.0;
    let left = rect.center().x - w / 2.0;
    p.hline(
        rect.left() + 8.0..=(left - 10.0).max(rect.left() + 8.0),
        rect.center().y,
        Stroke::new(1.0, pal.stroke),
    );
    p.hline(
        (left + w + 10.0).min(rect.right() - 8.0)..=rect.right() - 8.0,
        rect.center().y,
        Stroke::new(1.0, pal.stroke),
    );
    paint_icon(
        p,
        egui::pos2(left + 7.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        14.0,
        color,
    );
    p.galley(
        egui::pos2(left + 22.0, rect.center().y - g.size().y / 2.0),
        g,
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            call_id: id.to_owned(),
            name: "bash".to_owned(),
            intent: None,
            summary: "ls".to_owned(),
            args: String::new(),
        }
    }

    fn result(id: &str, is_error: bool) -> TranscriptItem {
        TranscriptItem::ToolResult {
            call_id: id.to_owned(),
            name: "bash".to_owned(),
            text: "out".to_owned(),
            is_error,
            truncated: false,
        }
    }

    #[test]
    fn appends_keep_row_caches_but_a_rewrite_drops_them() {
        let mut v = TranscriptView::new(false);
        v.sync(1, || Arc::new(vec![call("a"), call("b")]));
        v.flipped.insert(1);
        v.sync(2, || Arc::new(vec![call("a"), call("b"), call("c")]));
        assert!(v.flipped.contains(&1), "append must keep open/closed state");
        v.sync(3, || {
            Arc::new(vec![call("z"), call("x"), call("y"), call("w")])
        });
        assert!(v.flipped.is_empty(), "a longer rewrite must reset");
    }

    #[test]
    fn results_merge_into_their_call_and_empty_text_is_dropped() {
        let items = vec![
            TranscriptItem::Assistant {
                text: "  ".to_owned(),
                model: None,
                at: None,
            },
            call("a"),
            call("b"),
            result("b", false),
            TranscriptItem::Thinking {
                text: String::new(),
            },
            result("a", true),
            result("orphan", false),
            TranscriptItem::Exit {
                reason: "dispose".to_owned(),
            },
        ];
        let rows = build_rows(&items);
        assert_eq!(
            rows,
            vec![
                Row::Tool {
                    call: 1,
                    result: Some(5)
                },
                Row::Tool {
                    call: 2,
                    result: Some(3)
                },
                Row::Single(6),
                Row::Single(7),
            ]
        );
        assert!(default_open(&items, rows[0]), "failed tools start open");
        assert!(!default_open(&items, rows[1]));
    }

    #[test]
    fn pending_call_stays_open_ended_until_its_result_arrives() {
        let mut items = vec![call("a")];
        assert_eq!(
            build_rows(&items),
            vec![Row::Tool {
                call: 0,
                result: None
            }]
        );
        items.push(result("a", false));
        assert_eq!(
            build_rows(&items),
            vec![Row::Tool {
                call: 0,
                result: Some(1)
            }]
        );
    }

    #[test]
    fn estimates_grow_with_text() {
        let short = vec![TranscriptItem::Assistant {
            text: "hi".to_owned(),
            model: None,
            at: None,
        }];
        let long = vec![TranscriptItem::Assistant {
            text: "word ".repeat(600),
            model: None,
            at: None,
        }];
        assert!(estimate(&long, Row::Single(0), 700.0) > estimate(&short, Row::Single(0), 700.0));
    }
}
