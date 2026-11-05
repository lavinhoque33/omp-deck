//! The popup's transcript tab: a chat-style rendering of a subagent's [`TranscriptItem`]s.
//!
//! Thousands of items stay cheap: only rows intersecting the viewport are laid out. Row heights
//! are measured as rows are drawn (estimated before that) and kept in a prefix-sum table, so the
//! scrollbar is stable and a repaint is only requested when a measurement moved the layout.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::chat_view::MdCx;
use crate::chat_view::{
    MdBurst, MdStandalone, MdStandaloneOut, begin_md_standalone, decode_image_bytes, md_mono_run,
    md_show_text,
};
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

fn mono_block(
    ui: &mut Ui,
    burst: &mut MdBurst<'_>,
    idx: usize,
    text: &str,
    color: Color32,
    salt: (usize, &'static str),
    max_h: f32,
) {
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
                    // One `tsel` run of the row's node: selectable, and collected for copies.
                    burst.mono(ui, idx, text, color);
                });
        });
}

/// Largest preview side (px) before the image is scaled to fit.
const PREVIEW_SIDE: f32 = 240.0;

/// `bytes` as `12 B`, `1.2 KiB`, `3.4 MiB`.
fn fmt_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KiB", b / K)
    } else {
        format!("{:.1} MiB", b / (K * K))
    }
}

/// Short media label: `png`, `jpeg`, `webp`, else the subtype verbatim.
fn short_mime(mime: &str) -> &str {
    match mime.strip_prefix("image/") {
        Some(rest) => rest.split(';').next().unwrap_or(rest),
        None => mime,
    }
}

/// A small chip for a missing or unreadable image: `image · png · 12 KiB`.
fn image_chip(ui: &mut Ui, mime: &str, bytes: u64) {
    let pal = Palette::of_ui(ui);
    let label = if bytes > 0 {
        format!("image · {} · {}", short_mime(mime), fmt_bytes(bytes))
    } else {
        format!("image · {}", short_mime(mime))
    };
    Frame::NONE
        .fill(theme::mix(pal.surface, pal.text, 0.06))
        .stroke(Stroke::new(1.0, pal.stroke.gamma_multiply(0.8)))
        .corner_radius(theme::rad(6, 8))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(12.0).color(pal.weak));
        });
    ui.add_space(4.0);
}

/// `text` with the reader's `[image]` placeholder lines removed: the real previews render
/// below the text now, so the placeholder would double the image's presence.
fn strip_image_placeholders(text: &str) -> String {
    text.lines()
        .filter(|l| l.trim() != "[image]")
        .collect::<Vec<_>>()
        .join("\n")
}

/// Inline previews of a message's images: bounded, click to enlarge; a missing or unreadable
/// blob draws [`image_chip`] instead.
fn image_row(
    view: &mut RowKit<'_>,
    ui: &mut Ui,
    images: &[deck_core::agents::ImageRef],
    salt: &str,
) {
    if images.is_empty() {
        return;
    }
    ui.add_space(6.0);
    for (k, img) in images.iter().enumerate() {
        let tex = view.image_texture(ui.ctx(), &img.blob);
        match tex {
            Some(Some(tex)) => {
                let size = tex.size_vec2();
                let bounds = vec2(
                    ui.available_width().clamp(1.0, PREVIEW_SIDE * 2.0),
                    PREVIEW_SIDE,
                );
                let draw = size * (bounds.x / size.x).min(bounds.y / size.y).min(1.0);
                let (rect, resp) = ui.allocate_exact_size(draw, Sense::click());
                ui.painter().image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                let pal = Palette::of_ui(ui);
                ui.painter().rect_stroke(
                    rect,
                    theme::rad(6, 8),
                    Stroke::new(1.0, pal.stroke.gamma_multiply(0.8)),
                    egui::StrokeKind::Inside,
                );
                let resp = resp
                    .on_hover_cursor(CursorIcon::ZoomIn)
                    .on_hover_text(format!(
                        "{} · {}",
                        short_mime(&img.mime),
                        fmt_bytes(img.bytes)
                    ));
                if resp.clicked() {
                    *view.zoomed = Some(tex);
                }
                let _ = (salt, k);
            }
            Some(None) => image_chip(ui, &img.mime, img.bytes),
            None => {
                // Still loading: a quiet placeholder the preview replaces.
                let pal = Palette::of_ui(ui);
                ui.horizontal(|ui| {
                    ui.add(Spin::new(12.0, pal.weak));
                    ui.label(RichText::new("Loading image…").size(12.0).color(pal.weak));
                });
                ui.add_space(4.0);
            }
        }
    }
}

/// Mono size the chat renderer follows in the popup: it has no terminal pane, so a fixed
/// reading size (the same 13 px the chat tests use).
const MONO: f32 = 13.0;

/// Markdown through the chat view's renderer (one renderer for both surfaces): wrapped tables,
/// code blocks and theme ink. Draws inside the row's burst (`idx` is the row's node ord), so a
/// drag across rows selects across them. Returns a clicked link, if any.
fn markdown(
    ui: &mut Ui,
    md: &mut MdBurst<'_>,
    idx: usize,
    key: &str,
    gen_: u64,
    text: &str,
) -> MdStandaloneOut {
    md.draw(ui, idx, key, gen_, text);
    // The burst accumulates links; the row reads them at `finish`. Until then there is nothing
    // to report per message (links are drained once per frame, below).
    MdStandaloneOut::default()
}

/// `text` as markdown with its wrapper blocks (`<system-notice>`, `<context>`, …) drawn as
/// captioned subsections instead of literal tags. Clicked links accumulate in `out`.
#[allow(clippy::too_many_arguments)]
fn rich_text(
    ui: &mut Ui,
    md: &mut MdBurst<'_>,
    idx: usize,
    key: &str,
    gen_: u64,
    text: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    let pal = Palette::of_ui(ui);
    for (s, seg) in markup::segments(text).into_iter().enumerate() {
        match seg {
            Segment::Text(t) => {
                let r = markdown(
                    ui,
                    md,
                    idx,
                    &format!("{key}/t{s}"),
                    gen_,
                    t.trim_matches('\n'),
                );
                out.extend(r.open_url);
            }
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
                                md.mono(ui, idx, body, pal.text);
                            } else {
                                rich_text(
                                    ui,
                                    md,
                                    idx,
                                    &format!("{key}/g{s}"),
                                    gen_,
                                    body,
                                    depth + 1,
                                    out,
                                );
                            }
                        });
                }
                ui.add_space(6.0);
            }
        }
    }
}

/// Re-renders row `idx`'s selectable text invisibly for a copy: the same runs, in the same
/// node (ord `idx`) and order, as the visible frame drew. Headers, previews, captions and
/// images are chrome and contribute nothing; collapsed bodies are collected whole (copies see
/// past folds, like the chat's).
fn collect_row(cx: &MdCx, ui: &mut Ui, items: &[TranscriptItem], idx: usize, row: Row, rev: u64) {
    cx.in_node(idx as u32 + 1, || match row {
        Row::Tool { call, result } => {
            let (Some(c), r) = (items.get(call), result.and_then(|r| items.get(r))) else {
                return;
            };
            let TranscriptItem::ToolCall { args, .. } = c else {
                return;
            };
            if !args.trim().is_empty() {
                collect_mono(cx, ui, args);
            }
            if let Some(TranscriptItem::ToolResult { text, images, .. }) = r {
                let shown = if images.is_empty() {
                    text.clone()
                } else {
                    strip_image_placeholders(text)
                };
                if !shown.trim().is_empty() {
                    collect_mono(cx, ui, &shown);
                }
            }
        }
        Row::Single(i) => {
            let Some(item) = items.get(i) else {
                return;
            };
            let key = format!("row/{i}");
            match item {
                TranscriptItem::User { text, images, .. } => {
                    let shown = if images.is_empty() {
                        markup::strip_preamble(text).to_owned()
                    } else {
                        strip_image_placeholders(markup::strip_preamble(text))
                    };
                    collect_rich(cx, ui, &key, rev, &shown, 0);
                }
                TranscriptItem::Assistant { text, .. } => {
                    md_show_text(cx, ui, &key, rev, text, &[], false);
                }
                TranscriptItem::Thinking { .. }
                | TranscriptItem::ModelChange { .. }
                | TranscriptItem::Exit { .. }
                | TranscriptItem::ToolCall { .. } => {}
                TranscriptItem::ToolResult { text, images, .. } => {
                    let shown = if images.is_empty() {
                        text.clone()
                    } else {
                        strip_image_placeholders(text)
                    };
                    if !shown.trim().is_empty() {
                        collect_mono(cx, ui, &shown);
                    }
                }
                TranscriptItem::Notice { text, .. } | TranscriptItem::Irc { text, .. } => {
                    collect_rich(cx, ui, &key, rev, text, 0);
                }
                TranscriptItem::Compaction { summary, .. } => {
                    md_show_text(cx, ui, &key, rev, summary, &[], false);
                }
                TranscriptItem::Error { .. } => {}
            }
        }
    });
}

/// [`collect_row`] for one mono run (tool args/results, verbatim bodies).
fn collect_mono(cx: &MdCx, ui: &mut Ui, text: &str) {
    md_mono_run(cx, ui, text, Color32::WHITE);
}

/// [`collect_row`] for one `rich_text` body: the same segments, keys and order as the visible
/// frame (captions and chrome contribute nothing).
fn collect_rich(cx: &MdCx, ui: &mut Ui, key: &str, rev: u64, text: &str, depth: usize) {
    for (s, seg) in markup::segments(text).into_iter().enumerate() {
        match seg {
            Segment::Text(t) => {
                md_show_text(
                    cx,
                    ui,
                    &format!("{key}/t{s}"),
                    rev,
                    t.trim_matches('\n'),
                    &[],
                    false,
                );
            }
            Segment::Tagged(t) => {
                let body = t.body.trim_matches('\n');
                if body.trim().is_empty() {
                    continue;
                }
                if t.name == "output" || depth >= MAX_TAG_DEPTH {
                    collect_mono(cx, ui, body);
                } else {
                    collect_rich(cx, ui, &format!("{key}/g{s}"), rev, body, depth + 1);
                }
            }
        }
    }
}

/// What the image loader knows about one blob.
enum ImageSlot {
    /// Requested; the worker answers through `images_rx`.
    Pending,
    /// Decoded (or permanently failed): `None` draws the placeholder chip.
    Done(Option<egui::TextureHandle>),
}

/// One worker answer: the blob id plus its decoded RGBA image (or `None` when the blob is
/// missing, unreadable, or not an image).
struct ImageLoaded {
    blob: String,
    img: Option<egui::ColorImage>,
}

/// The pieces a row draw needs, borrowed disjointly from the view: the burst owns `md` while
/// the rows draw, so the rows take everything else through this handle.
struct RowKit<'a> {
    flipped: &'a mut HashSet<usize>,
    revision: Option<u64>,
    transcript: &'a PathBuf,
    images: &'a mut HashMap<String, ImageSlot>,
    images_rx: &'a mut Option<Receiver<ImageLoaded>>,
    images_tx: &'a mut Option<std::sync::mpsc::Sender<ImageLoaded>>,
    zoomed: &'a mut Option<egui::TextureHandle>,
}

impl RowKit<'_> {
    fn is_open(&self, items: &[TranscriptItem], i: usize, row: Row) -> bool {
        default_open(items, row) ^ self.flipped.contains(&i)
    }

    fn toggle(&mut self, i: usize) {
        if !self.flipped.remove(&i) {
            self.flipped.insert(i);
        }
    }

    /// The texture of `blob`, requesting its load on first sight. Reads and decodes on a
    /// worker; the UI thread only uploads the finished RGBA image (see
    /// [`TranscriptView::poll_images`]).
    fn image_texture(
        &mut self,
        ctx: &egui::Context,
        blob: &str,
    ) -> Option<Option<egui::TextureHandle>> {
        match self.images.get(blob) {
            Some(ImageSlot::Done(tex)) => return Some(tex.clone()),
            Some(ImageSlot::Pending) => return None,
            None => {}
        }
        self.images.insert(blob.to_owned(), ImageSlot::Pending);
        if self.images_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            *self.images_tx = Some(tx);
            *self.images_rx = Some(rx);
        }
        let tx = (*self.images_tx).clone();
        let path = (*self.transcript).clone();
        let owned = blob.to_owned();
        let ctx = ctx.clone();
        let send_blob = owned.clone();
        let spawned = std::thread::Builder::new()
            .name("deck-transcript-image".into())
            .spawn(move || {
                let bytes = deck_core::agents::read_blob(&path, &send_blob, 20 << 20)
                    .ok()
                    .flatten()
                    .map(|(_, b)| b);
                let img = bytes.as_deref().and_then(decode_image_bytes);
                if let Some(tx) = tx {
                    let _ = tx.send(ImageLoaded {
                        blob: send_blob,
                        img,
                    });
                }
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.images.insert(owned, ImageSlot::Done(None));
        }
        None
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
    md: MdStandalone,
    transcript: PathBuf,
    images: HashMap<String, ImageSlot>,
    images_rx: Option<Receiver<ImageLoaded>>,
    images_tx: Option<std::sync::mpsc::Sender<ImageLoaded>>,
    zoomed: Option<egui::TextureHandle>,
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
            md: MdStandalone::default(),
            transcript: PathBuf::new(),
            images: HashMap::new(),
            images_rx: None,
            images_tx: None,
            zoomed: None,
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

    /// Draw the transcript; `running` animates pending tool calls. `transcript` is the file the
    /// item images are read from. Returns a link the reader clicked, if any (at most one per
    /// /// frame: a second click lands next frame).
    pub fn show(
        &mut self,
        ui: &mut Ui,
        running: bool,
        transcript: &std::path::Path,
    ) -> Option<String> {
        if self.transcript.as_os_str() != transcript.as_os_str() {
            self.transcript = transcript.to_owned();
            self.images.clear();
        }
        self.poll_images(ui);
        // Copy chords (Ctrl+C / Ctrl+Shift+C, the app's own Copy event) with a selection: the
        // burst answers the copy at its start next frame, before the rows draw.
        self.copy_keys(ui);
        let items = Arc::clone(&self.items);
        let mut moved = false;
        let mut opened = None;
        // One burst for the whole frame: row `i` draws under node ord `i`, so a drag selects
        // across rows (and across a row's tag sections) like the chat selects across blocks.
        // The burst borrows `self.md` while the scroll closure borrows the other pieces: split
        // the borrows up front so neither closure takes the whole view.
        let ids: Vec<String> = (0..self.rows.len()).map(|i| format!("row/{i}")).collect();
        let items_c = Arc::clone(&items);
        let rows_c = self.rows.clone();
        let rev = self.revision.unwrap_or(0);
        let width = ui.available_width();
        let TranscriptView {
            rows,
            heights,
            offsets,
            width: view_w,
            flipped,
            follow,
            jump,
            md,
            revision,
            transcript,
            images,
            images_rx,
            images_tx,
            zoomed,
            ..
        } = self;
        let mut burst = begin_md_standalone(md, ui, MONO, &ids, width, |cx, ui, idx| {
            if let Some(row) = rows_c.get(idx).copied() {
                collect_row(cx, ui, &items_c, idx, row, rev);
            }
        });
        let mut kit = RowKit {
            flipped,
            revision: *revision,
            transcript,
            images,
            images_rx,
            images_tx,
            zoomed,
        };
        let out = ScrollArea::vertical()
            .id_salt("agent-transcript")
            .auto_shrink([false, false])
            .stick_to_bottom(*follow)
            .show_viewport(ui, |ui, viewport| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let w = ui.available_width();
                if (w - *view_w).abs() > 1.0 {
                    *view_w = w;
                    for (h, row) in heights.iter_mut().zip(rows.iter()) {
                        *h = estimate(&items, *row, *view_w);
                    }
                }
                offsets.clear();
                offsets.push(0.0);
                let mut y = 0.0;
                for h in heights.iter() {
                    y += h + GAP;
                    offsets.push(y);
                }
                let total = y;
                let n = rows.len();
                let lo = viewport.min.y - OVERSCAN;
                let hi = viewport.max.y + OVERSCAN;
                let first = offsets[1..].partition_point(|&o| o <= lo).min(n);
                let end = offsets[..n].partition_point(|&o| o < hi).max(first);
                ui.add_space(offsets[first]);
                for i in first..end {
                    let row = rows[i];
                    let h = ui
                        .scope(|ui| {
                            let mut links = Vec::new();
                            Self::draw_row(
                                &mut kit, ui, &mut burst, &items, i, row, running, &mut links,
                            );
                            if opened.is_none() {
                                opened = links.into_iter().next();
                            }
                        })
                        .response
                        .rect
                        .height();
                    if (h - heights[i]).abs() > 0.5 {
                        heights[i] = h;
                        moved = true;
                    }
                    ui.add_space(GAP);
                }
                ui.add_space((total - offsets[end]).max(0.0));
                let (marker, _) = ui.allocate_exact_size(vec2(1.0, 1.0), Sense::hover());
                if *jump {
                    ui.scroll_to_rect(marker, Some(Align::BOTTOM));
                    *jump = false;
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
        let r = burst.finish(ui, egui::Id::unique("transcript-md"));
        if opened.is_none() {
            opened = r.open_url;
        }
        self.zoom_modal(ui.ctx());
        opened
    }

    /// Copy chords while a selection lives: Ctrl+C / Ctrl+Shift+C and the app's Copy event.
    /// The burst answers at its start next frame (re-rendering the selected rows invisibly),
    /// the way the chat answers `want_copy` before its transcript draws. Esc clears.
    fn copy_keys(&mut self, ui: &mut Ui) {
        let has = self.md.has_selection();
        if !has {
            return;
        }
        let mut copy = false;
        let mut clear = false;
        ui.input(|i| {
            for e in &i.events {
                match e {
                    egui::Event::Copy => copy = true,
                    egui::Event::Key {
                        key: egui::Key::C,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command && !modifiers.alt => copy = true,
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        pressed: true,
                        modifiers,
                        ..
                    } if !modifiers.any() => clear = true,
                    _ => {}
                }
            }
        });
        if copy {
            self.md.want_copy();
        }
        if clear {
            self.md.clear_selection();
        }
    }

    /// Moves finished worker decodes into textures (uploading to the GPU is a UI-thread call).
    fn poll_images(&mut self, ui: &Ui) {
        let Some(rx) = &self.images_rx else {
            return;
        };
        let mut landed = false;
        loop {
            match rx.try_recv() {
                Ok(loaded) => {
                    landed = true;
                    let tex = loaded.img.map(|img| {
                        ui.ctx().load_texture(
                            format!("transcript-blob-{}", loaded.blob),
                            img,
                            egui::TextureOptions::LINEAR,
                        )
                    });
                    self.images.insert(loaded.blob, ImageSlot::Done(tex));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.images_rx = None;
                    self.images_tx = None;
                    break;
                }
            }
        }
        if landed {
            // Previews change row heights; measured as drawn next frame.
            ui.ctx().request_repaint();
        }
    }

    /// The enlarged preview while one is open; true while it stays open.
    fn zoom_modal(&mut self, ctx: &egui::Context) {
        let Some(tex) = self.zoomed.clone() else {
            return;
        };
        let bounds = ctx.content_rect().size() - vec2(64.0, 144.0);
        let size = tex.size_vec2();
        let k = (bounds.x.max(1.0) / size.x)
            .min(bounds.y.max(1.0) / size.y)
            .min(4.0);
        let draw = size * k;
        let head = crate::card::Header::new(crate::icons::ICON_APP_WINDOW, "Image preview");
        if crate::card::modal(
            ctx,
            egui::Id::unique("transcript-image-zoom"),
            draw.x + 32.0,
            &head,
            |ui| {
                ui.add(egui::Image::new(&tex).fit_to_exact_size(draw));
            },
        ) {
            self.zoomed = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_row(
        view: &mut RowKit<'_>,
        ui: &mut Ui,
        burst: &mut MdBurst<'_>,
        items: &[TranscriptItem],
        i: usize,
        row: Row,
        running: bool,
        links: &mut Vec<String>,
    ) {
        let pal = Palette::of_ui(ui);
        let open = view.is_open(items, i, row);
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
                let res_idx =
                    result.filter(|r| matches!(&items[*r], TranscriptItem::ToolResult { .. }));
                let res = res_idx.map(|r| match &items[r] {
                    TranscriptItem::ToolResult {
                        text,
                        is_error,
                        truncated,
                        images,
                        ..
                    } => (text.as_str(), *is_error, *truncated, !images.is_empty()),
                    _ => ("", false, false, false),
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
                    Some((_, true, _, _)) => Badge::Icon(ICON_CIRCLE_ALERT, pal.danger),
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
                            mono_block(ui, burst, i, args, pal.weak, (i, "args"), 120.0);
                            ui.add_space(6.0);
                        }
                        match res {
                            Some((text, is_error, truncated, has_images)) => {
                                caption(
                                    ui,
                                    if is_error { "Error" } else { "Result" },
                                    if is_error { pal.danger } else { pal.weak },
                                );
                                let cleaned;
                                let body = if text.trim().is_empty() {
                                    "(empty)"
                                } else if has_images {
                                    cleaned = strip_image_placeholders(text);
                                    cleaned.as_str()
                                } else {
                                    text
                                };
                                mono_block(
                                    ui,
                                    burst,
                                    i,
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
                    view.toggle(i);
                }
                if open
                    && let Some(r) = res_idx
                    && let TranscriptItem::ToolResult { images, .. } = &items[r]
                    && !images.is_empty()
                {
                    // Below the bubble, indented to its text: measured as part of the row.
                    Frame::NONE
                        .inner_margin(Margin {
                            left: 18,
                            right: 8,
                            top: 0,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let salt = format!("tool/{i}");
                            // Clone the refs: `image_row` needs `&mut self`.
                            let images = images.clone();
                            image_row(view, ui, &images, &salt);
                        });
                }
            }
            Row::Single(idx) => {
                Self::draw_single(view, ui, burst, &items[idx], i, open, margin, links)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_single(
        view: &mut RowKit<'_>,
        ui: &mut Ui,
        burst: &mut MdBurst<'_>,
        item: &TranscriptItem,
        i: usize,
        open: bool,
        margin: Margin,
        links: &mut Vec<String>,
    ) {
        let pal = Palette::of_ui(ui);
        match item {
            TranscriptItem::User {
                text,
                from_agent,
                images,
                ..
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
                                    let key = format!("row/{i}");
                                    let gen_ = view.revision.unwrap_or(0);
                                    // `item` borrows the caller; clone the small refs instead.
                                    let images = images.clone();
                                    let shown = if images.is_empty() {
                                        markup::strip_preamble(text).to_owned()
                                    } else {
                                        strip_image_placeholders(markup::strip_preamble(text))
                                    };
                                    rich_text(ui, burst, i, &key, gen_, &shown, 0, links);
                                    image_row(view, ui, &images, &key);
                                });
                        }
                        c
                    },
                );
                if clicked {
                    view.toggle(i);
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
                    let r = markdown(
                        ui,
                        burst,
                        i,
                        &format!("row/{i}"),
                        view.revision.unwrap_or(0),
                        text,
                    );
                    links.extend(r.open_url);
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
                    view.toggle(i);
                }
            }
            TranscriptItem::ToolResult {
                name,
                text,
                is_error,
                images,
                ..
            } => {
                let shown = if images.is_empty() {
                    text.clone()
                } else {
                    strip_image_placeholders(text)
                };
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
                            burst,
                            i,
                            &shown,
                            if *is_error { pal.danger } else { pal.text },
                            (i, "orphan"),
                            RESULT_MAX_H,
                        );
                    },
                );
                if !images.is_empty() {
                    Frame::NONE
                        .inner_margin(Margin {
                            left: 18,
                            right: 8,
                            top: 0,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let key = format!("row/{i}");
                            let images = images.clone();
                            image_row(view, ui, &images, &key);
                        });
                }
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
                                    let key = format!("row/{i}");
                                    let gen_ = view.revision.unwrap_or(0);
                                    rich_text(ui, burst, i, &key, gen_, text, 0, links);
                                });
                        }
                        c
                    },
                );
                if clicked {
                    view.toggle(i);
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
                                    let key = format!("row/{i}");
                                    let gen_ = view.revision.unwrap_or(0);
                                    rich_text(ui, burst, i, &key, gen_, text, 0, links);
                                });
                        }
                        c
                    },
                );
                if clicked {
                    view.toggle(i);
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
                            let r = markdown(
                                ui,
                                burst,
                                i,
                                &format!("row/{i}"),
                                view.revision.unwrap_or(0),
                                summary,
                            );
                            links.extend(r.open_url);
                        });
                }
                if clicked {
                    view.toggle(i);
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
    use egui::{Pos2, Rect};
    use std::time::Duration;
    fn call(id: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            call_id: id.to_owned(),
            name: "bash".to_owned(),
            intent: None,
            summary: "ls".to_owned(),
            args: String::new(),
            args_value: None,
        }
    }

    fn result(id: &str, is_error: bool) -> TranscriptItem {
        TranscriptItem::ToolResult {
            call_id: id.to_owned(),
            name: "bash".to_owned(),
            text: "out".to_owned(),
            is_error,
            truncated: false,
            details: None,
            images: Vec::new(),
        }
    }

    /// A context with the app's theme and fonts installed (the rows use named text styles).
    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);
        crate::theme::apply(
            &ctx,
            deck_core::config::UiStyle::Blueprint,
            deck_core::config::Density::default(),
            deck_core::config::UiFont::Auto,
            true,
        );
        ctx
    }

    /// One frame of the view in a `w`×600 window; the shapes it painted.
    fn frame(
        v: &mut TranscriptView,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        transcript: &std::path::Path,
        w: f32,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(w, 600.0))),
            events,
            focused: true,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            let _ = v.show(ui, false, transcript);
        });
        out.textures_delta.clear();
        out
    }

    /// Painted galleys: `(pos, size, text)` of every text shape.
    fn galleys(out: &egui::FullOutput) -> Vec<(Pos2, egui::Vec2, String)> {
        fn walk(shape: &egui::Shape, hits: &mut Vec<(Pos2, egui::Vec2, String)>) {
            match shape {
                egui::Shape::Text(t) => hits.push((t.pos, t.galley.size(), t.galley.text().into())),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, hits)),
                _ => {}
            }
        }
        let mut hits = Vec::new();
        for clipped in &out.shapes {
            walk(&clipped.shape, &mut hits);
        }
        hits
    }

    /// Copied texts (`CopyText` commands) of a frame.
    fn copied(out: &egui::FullOutput) -> Vec<String> {
        out.platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pointer events: a move to `at`, plus a primary press/release when asked.
    fn pointer(at: Pos2, pressed: Option<bool>) -> Vec<egui::Event> {
        let mut events = vec![egui::Event::PointerMoved(at)];
        if let Some(pressed) = pressed {
            events.push(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        events
    }

    fn ctrl_c(pressed: bool) -> egui::Event {
        egui::Event::Key {
            key: egui::Key::C,
            physical_key: Some(egui::Key::C),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        }
    }

    /// A drag across two assistant rows copies both rows' words (and a single Ctrl+C only ever
    /// copies one text).
    #[test]
    fn a_drag_across_rows_copies_both() {
        let transcript = std::path::PathBuf::new();
        let mut v = TranscriptView::new(false);
        v.sync(1, || {
            Arc::new(vec![
                TranscriptItem::Assistant {
                    text: "alpha bravo".to_owned(),
                    model: None,
                    at: None,
                },
                TranscriptItem::Assistant {
                    text: "charlie delta".to_owned(),
                    model: None,
                    at: None,
                },
            ])
        });
        let ctx = ctx();
        let out = frame(&mut v, &ctx, vec![], &transcript, 700.0);
        let (pa, sa, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2.contains("alpha"))
            .expect("first row painted");
        let (pb, sb, _) = galleys(&frame(&mut v, &ctx, vec![], &transcript, 700.0))
            .into_iter()
            .find(|g| g.2.contains("charlie"))
            .expect("second row painted");
        let (a, b) = (
            pa + vec2(3.0, sa.y / 2.0),
            pb + vec2(sb.x - 3.0, sb.y / 2.0),
        );
        frame(&mut v, &ctx, pointer(a, None), &transcript, 700.0);
        frame(&mut v, &ctx, pointer(a, Some(true)), &transcript, 700.0);
        frame(&mut v, &ctx, pointer(b, None), &transcript, 700.0);
        frame(&mut v, &ctx, pointer(b, Some(false)), &transcript, 700.0);
        assert!(v.md.has_selection(), "the drag selected across rows");
        let got = copied(&frame(&mut v, &ctx, vec![ctrl_c(true)], &transcript, 700.0));
        assert_eq!(got.len(), 1, "one copy for one Ctrl+C");
        assert!(
            got[0].contains("alpha") && got[0].contains("delta"),
            "both rows' words copied: {:?}",
            got[0]
        );
    }

    /// A click on a preview opens the zoom modal; settling the modal's close dismisses it.
    /// The worker path (blob → decode → texture) is proved by the visual harness; here the
    /// decoded texture is placed directly, so the test is the click → modal wiring only.
    #[test]
    fn a_click_enlarges_the_preview() {
        use egui::Shape;
        let transcript = std::path::PathBuf::new();
        let mut v = TranscriptView::new(false);
        v.sync(1, || {
            Arc::new(vec![TranscriptItem::User {
                text: "see".to_owned(),
                from_agent: false,
                at: None,
                images: vec![deck_core::agents::ImageRef {
                    blob: "b".to_owned(),
                    mime: "image/png".to_owned(),
                    bytes: 4,
                }],
            }])
        });
        let ctx = ctx();
        let tex = ctx.load_texture(
            "test-zoom",
            egui::ColorImage::filled([8, 8], egui::Color32::BLUE),
            egui::TextureOptions::LINEAR,
        );
        v.images.insert("b".to_owned(), ImageSlot::Done(Some(tex)));
        let out = frame(&mut v, &ctx, vec![], &transcript, 700.0);
        fn image_at(out: &egui::FullOutput) -> Option<egui::Rect> {
            // `painter().image` paints a textured `Mesh`; the preview is the only textured
            // mesh of this frame.
            fn walk(shape: &Shape, hits: &mut Vec<egui::Rect>) {
                match shape {
                    Shape::Mesh(m) if !matches!(m.texture_id, egui::TextureId::Managed(0)) => {
                        hits.push(m.calc_bounds());
                    }
                    Shape::Vec(v) => v.iter().for_each(|s| walk(s, hits)),
                    _ => {}
                }
            }
            let mut hits = Vec::new();
            for clipped in &out.shapes {
                walk(&clipped.shape, &mut hits);
            }
            hits.into_iter().next()
        }
        let rect = image_at(&out).expect("the preview paints");
        let at = rect.center();
        frame(&mut v, &ctx, pointer(at, Some(true)), &transcript, 700.0);
        frame(&mut v, &ctx, pointer(at, Some(false)), &transcript, 700.0);
        assert!(v.zoomed.is_some(), "the click opened the zoom modal");
        let out = frame(&mut v, &ctx, vec![], &transcript, 700.0);
        assert!(
            galleys(&out).iter().any(|g| g.2 == "Image preview"),
            "the modal paints its header"
        );
    }

    /// A blob that never resolves draws the placeholder chip, not a spinner forever.
    #[test]
    fn a_missing_blob_draws_the_chip() {
        let transcript = std::path::PathBuf::from("/nonexistent/transcript.jsonl");
        let mut v = TranscriptView::new(false);
        v.sync(1, || {
            Arc::new(vec![TranscriptItem::User {
                text: "see".to_owned(),
                from_agent: false,
                at: None,
                images: vec![deck_core::agents::ImageRef {
                    blob: "missing".to_owned(),
                    mime: "image/png".to_owned(),
                    bytes: 12,
                }],
            }])
        });
        let ctx = ctx();
        let mut out = frame(&mut v, &ctx, vec![], &transcript, 700.0);
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(20));
            out = frame(&mut v, &ctx, vec![], &transcript, 700.0);
            if galleys(&out).iter().any(|g| g.2.contains("image · png")) {
                return;
            }
        }
        panic!(
            "the chip never painted: {:?}",
            galleys(&out).iter().map(|g| &g.2).collect::<Vec<_>>()
        );
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
